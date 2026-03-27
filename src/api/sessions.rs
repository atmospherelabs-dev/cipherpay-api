use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::scanner::{decrypt, mempool};

const SESSION_MEMO_PREFIX: &str = "zipher:session:";

#[derive(Debug, Deserialize)]
pub struct OpenRequest {
    pub txid: String,
    pub merchant_id: String,
    pub refund_address: Option<String>,
}

pub async fn open(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    http_client: web::Data<reqwest::Client>,
    body: web::Json<OpenRequest>,
) -> HttpResponse {
    if body.txid.len() != 64 || !body.txid.chars().all(|c| c.is_ascii_hexdigit()) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid txid format"
        }));
    }

    if let Some(ref addr) = body.refund_address {
        if !addr.is_empty() {
            if let Err(e) = crate::validation::validate_zcash_address("refund_address", addr) {
                return HttpResponse::BadRequest().json(e.to_json());
            }
        }
    }

    if crate::sessions::txid_already_used(pool.get_ref(), &body.txid).await {
        return HttpResponse::Conflict().json(serde_json::json!({
            "error": "This transaction has already been used to open a session"
        }));
    }

    let merchant = match crate::merchants::get_merchant_by_id(
        pool.get_ref(), &body.merchant_id, &config.encryption_key
    ).await {
        Ok(Some(m)) => m,
        _ => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Merchant not found"
            }));
        }
    };

    let raw_hex = match mempool::fetch_raw_tx(&http_client, &config.cipherscan_api_url, &body.txid).await {
        Ok(hex) => hex,
        Err(e) => {
            tracing::warn!(txid = %body.txid, error = %e, "session: failed to fetch tx");
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Transaction not found"
            }));
        }
    };

    let outputs = match decrypt::try_decrypt_all_outputs_ivk(&raw_hex, &merchant.ufvk) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "session: decryption failed");
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Could not decrypt transaction outputs for this merchant"
            }));
        }
    };

    if outputs.is_empty() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "No outputs addressed to this merchant"
        }));
    }

    let expected_memo = format!("{}{}", SESSION_MEMO_PREFIX, body.merchant_id);
    let session_outputs: Vec<_> = outputs.iter()
        .filter(|o| o.memo.trim() == expected_memo)
        .collect();

    if session_outputs.is_empty() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("No output with session memo found. Expected memo: {}", expected_memo),
        }));
    }

    let total_zatoshis: i64 = session_outputs.iter()
        .map(|o| o.amount_zatoshis as i64)
        .sum();

    if total_zatoshis < 10_000 {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Deposit too small — minimum 10,000 zatoshis (0.0001 ZEC)"
        }));
    }

    match crate::sessions::create_session(
        pool.get_ref(),
        &body.merchant_id,
        &body.txid,
        total_zatoshis,
        body.refund_address.as_deref(),
    ).await {
        Ok(session) => {
            HttpResponse::Created().json(serde_json::json!({
                "session_id": session.id,
                "bearer_token": session.bearer_token,
                "balance": session.balance_remaining,
                "expires_at": session.expires_at,
                "cost_per_request": session.cost_per_request,
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to create session");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create session"
            }))
        }
    }
}

/// Resolve the merchant owning this session, requiring API key or dashboard auth.
async fn require_session_owner(
    req: &HttpRequest,
    pool: &SqlitePool,
    session_id: &str,
) -> Result<(), HttpResponse> {
    let session = crate::sessions::get_session(pool, session_id).await
        .map_err(|_| HttpResponse::InternalServerError().json(serde_json::json!({"error": "Internal error"})))?
        .ok_or_else(|| HttpResponse::NotFound().json(serde_json::json!({"error": "Session not found"})))?;

    // Try dashboard session auth
    if let Some(merchant) = crate::api::auth::resolve_session(req, &web::Data::new(pool.clone())).await {
        if merchant.id == session.merchant_id {
            return Ok(());
        }
    }

    // Try API key auth
    if let Some(auth_header) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            let key = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str).trim();
            let enc_key = req.app_data::<web::Data<Config>>()
                .map(|c| c.encryption_key.clone()).unwrap_or_default();
            if let Ok(Some(merchant)) = crate::merchants::authenticate(pool, key, &enc_key).await {
                if merchant.id == session.merchant_id {
                    return Ok(());
                }
            }
        }
    }

    Err(HttpResponse::Unauthorized().json(serde_json::json!({"error": "Not authorized for this session"})))
}

pub async fn get_status(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let session_id = path.into_inner();

    if let Err(resp) = require_session_owner(&req, pool.get_ref(), &session_id).await {
        return resp;
    }

    match crate::sessions::get_summary(pool.get_ref(), &session_id).await {
        Ok(Some(summary)) => {
            HttpResponse::Ok().json(serde_json::json!({
                "session_id": summary.session_id,
                "requests_made": summary.requests_made,
                "balance_used": summary.balance_used,
                "balance_remaining": summary.balance_remaining,
                "status": summary.status,
            }))
        }
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Session not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session status");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn close(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let session_id = path.into_inner();

    if let Err(resp) = require_session_owner(&req, pool.get_ref(), &session_id).await {
        return resp;
    }

    match crate::sessions::close_session(pool.get_ref(), &session_id).await {
        Ok(Some(summary)) => {
            let mut resp = serde_json::json!({
                "session_id": summary.session_id,
                "requests_made": summary.requests_made,
                "balance_used": summary.balance_used,
                "balance_remaining": summary.balance_remaining,
                "status": summary.status,
            });

            if let Some(addr) = &summary.refund_address {
                if summary.balance_remaining > 0 {
                    let refund_zec = summary.balance_remaining as f64 / 100_000_000.0;
                    resp.as_object_mut().unwrap().insert(
                        "refund".to_string(),
                        serde_json::json!({
                            "address": addr,
                            "amount_zatoshis": summary.balance_remaining,
                            "amount_zec": refund_zec,
                        }),
                    );
                }
            }

            HttpResponse::Ok().json(resp)
        }
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Session not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to close session");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn history(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match crate::api::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    match crate::sessions::list_for_merchant(pool.get_ref(), &merchant.id).await {
        Ok(sessions) => {
            let items: Vec<_> = sessions.iter().map(|s| {
                let balance_used = s.balance_zatoshis - s.balance_remaining;
                let mut obj = serde_json::json!({
                    "id": s.id,
                    "deposit_txid": s.deposit_txid,
                    "balance_zatoshis": s.balance_zatoshis,
                    "balance_remaining": s.balance_remaining,
                    "cost_per_request": s.cost_per_request,
                    "requests_made": s.requests_made,
                    "balance_used": balance_used,
                    "status": s.status,
                    "expires_at": s.expires_at,
                    "created_at": s.created_at,
                    "closed_at": s.closed_at,
                });

                if let Some(ref addr) = s.refund_address {
                    if s.balance_remaining > 0 && (s.status == "closed" || s.status == "depleted" || s.status == "expired") {
                        let refund_zec = s.balance_remaining as f64 / 100_000_000.0;
                        obj.as_object_mut().unwrap().insert("refund".to_string(), serde_json::json!({
                            "address": addr,
                            "amount_zatoshis": s.balance_remaining,
                            "amount_zec": refund_zec,
                        }));
                    }
                }
                obj
            }).collect();

            HttpResponse::Ok().json(serde_json::json!({ "sessions": items }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list sessions");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn validate(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    // Accept token from Authorization: Bearer header (preferred) or query param (legacy)
    let token = req.headers().get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("cps_"))
        .or_else(|| {
            req.query_string()
                .split('&')
                .find_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    if k == "token" { Some(v.to_string()) } else { None }
                })
        });

    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Bearer token required — use Authorization: Bearer header or token query parameter"
            }));
        }
    };

    match crate::sessions::validate_and_deduct(pool.get_ref(), &token).await {
        Ok(Some(session)) => {
            HttpResponse::Ok()
                .insert_header(("X-Session-Balance", session.balance_remaining.to_string()))
                .json(serde_json::json!({
                    "valid": true,
                    "session_id": session.id,
                    "balance_remaining": session.balance_remaining,
                    "requests_made": session.requests_made,
                }))
        }
        Ok(None) => {
            HttpResponse::Ok().json(serde_json::json!({
                "valid": false,
                "reason": "Session not found, expired, or depleted"
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Session validation error");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}
