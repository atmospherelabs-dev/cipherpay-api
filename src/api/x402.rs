use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;
use crate::merchants;
use crate::scanner::{decrypt, mempool};

const SLIPPAGE_TOLERANCE: f64 = 0.995;

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub txid: String,
    pub expected_amount_zec: f64,
}

#[derive(Debug, Serialize)]
struct VerifyResponse {
    valid: bool,
    received_zec: f64,
    received_zatoshis: u64,
    previously_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

pub async fn verify(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    http_client: web::Data<reqwest::Client>,
    body: web::Json<VerifyRequest>,
) -> HttpResponse {
    let api_key = match extract_api_key(&req) {
        Some(k) => k,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing or invalid Authorization header"
            }));
        }
    };

    let merchant = match merchants::authenticate(&pool, &api_key, &config.encryption_key).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid API key"
            }));
        }
        Err(e) => {
            tracing::error!(error = %e, "x402 auth error");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };

    if body.txid.len() != 64 || !body.txid.chars().all(|c| c.is_ascii_hexdigit()) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid txid format — expected 64 hex characters"
        }));
    }

    if body.expected_amount_zec <= 0.0 {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "expected_amount_zec must be positive"
        }));
    }

    let previously_verified = was_previously_verified(&pool, &merchant.id, &body.txid).await;

    let raw_hex = match mempool::fetch_raw_tx(&http_client, &config.cipherscan_api_url, &body.txid).await {
        Ok(hex) => hex,
        Err(e) => {
            tracing::warn!(txid = %body.txid, error = %e, "x402: failed to fetch raw tx");
            let resp = build_rejected(&pool, &merchant.id, &body.txid, 0, previously_verified, "Transaction not found").await;
            return HttpResponse::Ok().json(resp);
        }
    };

    let outputs = match decrypt::try_decrypt_all_outputs(&raw_hex, &merchant.ufvk) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(txid = %body.txid, error = %e, "x402: decryption error");
            let resp = build_rejected(&pool, &merchant.id, &body.txid, 0, previously_verified, "Decryption failed").await;
            return HttpResponse::Ok().json(resp);
        }
    };

    if outputs.is_empty() {
        let resp = build_rejected(&pool, &merchant.id, &body.txid, 0, previously_verified, "No outputs addressed to this merchant").await;
        return HttpResponse::Ok().json(resp);
    }

    let total_zatoshis: u64 = outputs.iter().map(|o| o.amount_zatoshis).sum();
    let total_zec = total_zatoshis as f64 / 100_000_000.0;
    let expected_zatoshis = (body.expected_amount_zec * 100_000_000.0) as u64;
    let min_acceptable = (expected_zatoshis as f64 * SLIPPAGE_TOLERANCE) as u64;

    if total_zatoshis >= min_acceptable {
        log_verification(&pool, &merchant.id, &body.txid, total_zatoshis, "verified", None).await;

        HttpResponse::Ok().json(VerifyResponse {
            valid: true,
            received_zec: total_zec,
            received_zatoshis: total_zatoshis,
            previously_verified,
            reason: None,
        })
    } else {
        let reason = format!(
            "Insufficient amount: received {} ZEC, expected {} ZEC",
            total_zec, body.expected_amount_zec
        );
        log_verification(&pool, &merchant.id, &body.txid, total_zatoshis, "rejected", Some(&reason)).await;

        HttpResponse::Ok().json(VerifyResponse {
            valid: false,
            received_zec: total_zec,
            received_zatoshis: total_zatoshis,
            previously_verified,
            reason: Some(reason),
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn history(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    query: web::Query<HistoryQuery>,
) -> HttpResponse {
    let merchant = match resolve_merchant(&req, &pool, &config).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0).max(0);

    let rows = sqlx::query_as::<_, (String, String, Option<i64>, Option<f64>, String, Option<String>, String)>(
        "SELECT id, txid, amount_zatoshis, amount_zec, status, reason, created_at
         FROM x402_verifications
         WHERE merchant_id = ?
         ORDER BY created_at DESC
         LIMIT ? OFFSET ?"
    )
    .bind(&merchant.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(rows) => {
            let items: Vec<_> = rows.into_iter().map(|r| {
                serde_json::json!({
                    "id": r.0,
                    "txid": r.1,
                    "amount_zatoshis": r.2,
                    "amount_zec": r.3,
                    "status": r.4,
                    "reason": r.5,
                    "created_at": r.6,
                })
            }).collect();
            HttpResponse::Ok().json(serde_json::json!({ "verifications": items }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch x402 history");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

/// Try session cookie first, then fall back to API key auth.
async fn resolve_merchant(
    req: &HttpRequest,
    pool: &SqlitePool,
    config: &Config,
) -> Option<merchants::Merchant> {
    if let Some(m) = super::auth::resolve_session(req, pool).await {
        return Some(m);
    }
    if let Some(key) = extract_api_key(req) {
        if let Ok(Some(m)) = merchants::authenticate(pool, &key, &config.encryption_key).await {
            return Some(m);
        }
    }
    None
}

fn extract_api_key(req: &HttpRequest) -> Option<String> {
    let header = req.headers().get("Authorization")?;
    let value = header.to_str().ok()?;
    let key = value.strip_prefix("Bearer ").unwrap_or(value).trim();
    if key.is_empty() { None } else { Some(key.to_string()) }
}

async fn build_rejected(
    pool: &SqlitePool,
    merchant_id: &str,
    txid: &str,
    zatoshis: u64,
    previously_verified: bool,
    reason: &str,
) -> VerifyResponse {
    log_verification(pool, merchant_id, txid, zatoshis, "rejected", Some(reason)).await;
    VerifyResponse {
        valid: false,
        received_zec: zatoshis as f64 / 100_000_000.0,
        received_zatoshis: zatoshis,
        previously_verified,
        reason: Some(reason.to_string()),
    }
}

async fn was_previously_verified(pool: &SqlitePool, merchant_id: &str, txid: &str) -> bool {
    sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM x402_verifications WHERE merchant_id = ? AND txid = ? AND status = 'verified'"
    )
    .bind(merchant_id)
    .bind(txid)
    .fetch_one(pool)
    .await
    .unwrap_or(0) > 0
}

async fn log_verification(
    pool: &SqlitePool,
    merchant_id: &str,
    txid: &str,
    amount_zatoshis: u64,
    status: &str,
    reason: Option<&str>,
) {
    let id = Uuid::new_v4().to_string();
    let amount_zec = amount_zatoshis as f64 / 100_000_000.0;

    let result = sqlx::query(
        "INSERT INTO x402_verifications (id, merchant_id, txid, amount_zatoshis, amount_zec, status, reason)
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(txid)
    .bind(amount_zatoshis as i64)
    .bind(amount_zec)
    .bind(status)
    .bind(reason)
    .execute(pool)
    .await;

    if let Err(e) = result {
        tracing::warn!(error = %e, "Failed to log x402 verification");
    }
}
