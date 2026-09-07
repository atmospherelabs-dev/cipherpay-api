use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

fn default_protocol() -> String {
    "x402".to_string()
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

    if config.fee_enabled() {
        if let Ok(status) =
            crate::billing::get_merchant_billing_status(pool.get_ref(), &merchant.id).await
        {
            if merchant_billing_blocked(&status) {
                return HttpResponse::PaymentRequired().json(serde_json::json!({
                    "error": "Merchant account has outstanding fees",
                    "billing_status": status,
                }));
            }
        }
    }

    if body.txid.len() != 64 || !body.txid.chars().all(|c| c.is_ascii_hexdigit()) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid txid format — expected 64 hex characters"
        }));
    }

    if !body.expected_amount_zec.is_finite() || body.expected_amount_zec <= 0.0 {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "expected_amount_zec must be positive"
        }));
    }

    let protocol = if body.protocol == "mpp" {
        "mpp"
    } else {
        "x402"
    };

    if let Some(received_zatoshis) =
        get_existing_verified(pool.get_ref(), &merchant.id, &body.txid, protocol).await
    {
        // Re-check amount against the current request to prevent replay across price tiers:
        // a $5 txid verified once must not pass as proof for a $500 resource.
        let expected_zatoshis = match zec_to_zatoshis(body.expected_amount_zec) {
            Some(amount) => amount,
            None => {
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "expected_amount_zec must be representable in zatoshis"
                }));
            }
        };
        let min_acceptable = expected_zatoshis;

        if received_zatoshis >= min_acceptable {
            return HttpResponse::Ok().json(VerifyResponse {
                valid: false,
                received_zec: received_zatoshis as f64 / 100_000_000.0,
                received_zatoshis,
                previously_verified: true,
                reason: Some("Payment already verified; use a new payment".to_string()),
            });
        } else {
            let reason = format!(
                "Previously verified amount insufficient: received {} ZEC, expected {} ZEC",
                received_zatoshis as f64 / 100_000_000.0,
                body.expected_amount_zec
            );
            return HttpResponse::Ok().json(VerifyResponse {
                valid: false,
                received_zec: received_zatoshis as f64 / 100_000_000.0,
                received_zatoshis,
                previously_verified: true,
                reason: Some(reason),
            });
        }
    }

    let previously_verified = false;

    if !crate::scanner::blocks::check_tx_confirmed(
        &http_client,
        &config.cipherscan_api_url,
        &body.txid,
    )
    .await
    .unwrap_or(false)
    {
        return HttpResponse::Ok().json(VerifyResponse {
            valid: false,
            received_zec: 0.0,
            received_zatoshis: 0,
            previously_verified: false,
            reason: Some("Payment is not confirmed".to_string()),
        });
    }

    let raw_hex =
        match mempool::fetch_raw_tx(&http_client, &config.cipherscan_api_url, &body.txid).await {
            Ok(hex) => hex,
            Err(e) => {
                tracing::warn!(txid = %body.txid, error = %e, "x402: failed to fetch raw tx");
                let resp = build_rejected(
                    &pool,
                    &merchant.id,
                    &body.txid,
                    0,
                    previously_verified,
                    "Transaction not found",
                    protocol,
                )
                .await;
                return HttpResponse::Ok().json(resp);
            }
        };

    let outputs = match decrypt::try_decrypt_all_outputs_ivk(&raw_hex, &merchant.ufvk) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(txid = %body.txid, error = %e, "x402: decryption error");
            let resp = build_rejected(
                &pool,
                &merchant.id,
                &body.txid,
                0,
                previously_verified,
                "Decryption failed",
                protocol,
            )
            .await;
            return HttpResponse::Ok().json(resp);
        }
    };

    if outputs.is_empty() {
        let resp = build_rejected(
            &pool,
            &merchant.id,
            &body.txid,
            0,
            previously_verified,
            "No outputs addressed to this merchant",
            protocol,
        )
        .await;
        return HttpResponse::Ok().json(resp);
    }

    let receiver = match crate::invoices::matching::orchard_receiver(&merchant.payment_address) {
        Some(r) => r,
        None => return HttpResponse::BadRequest().finish(),
    };
    let total_zatoshis: u64 = outputs
        .iter()
        .filter(|o| hex::encode(o.recipient_raw) == receiver)
        .map(|o| o.amount_zatoshis)
        .sum();
    let total_zec = total_zatoshis as f64 / 100_000_000.0;
    let expected_zatoshis = match zec_to_zatoshis(body.expected_amount_zec) {
        Some(amount) => amount,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "expected_amount_zec must be representable in zatoshis"
            }));
        }
    };
    let min_acceptable = expected_zatoshis;

    if total_zatoshis >= min_acceptable {
        let consumed = match pool.acquire().await {
            Ok(mut conn) => {
                crate::sessions::consume_payment(&mut conn, &body.txid, "legacy-x402").await
            }
            Err(e) => Err(e.into()),
        };
        match consumed {
            Ok(true) => {}
            Ok(false) => {
                return HttpResponse::Conflict().json(problem_details(
                    409,
                    "payment-replayed",
                    "Payment Replayed",
                    "Transaction already consumed",
                ))
            }
            Err(_) => return HttpResponse::InternalServerError().finish(),
        }
        log_verification(
            &pool,
            &merchant.id,
            &body.txid,
            total_zatoshis,
            "verified",
            None,
            protocol,
        )
        .await;

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
        log_verification(
            &pool,
            &merchant.id,
            &body.txid,
            total_zatoshis,
            "rejected",
            Some(&reason),
            protocol,
        )
        .await;

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

    let rows = sqlx::query_as::<_, (String, String, Option<i64>, Option<f64>, String, Option<String>, String, String)>(
        "SELECT id, txid, amount_zatoshis, amount_zec, status, reason, created_at, COALESCE(protocol, 'x402')
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
            let items: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.0,
                        "txid": r.1,
                        "amount_zatoshis": r.2,
                        "amount_zec": r.3,
                        "status": r.4,
                        "reason": r.5,
                        "created_at": r.6,
                        "protocol": r.7,
                    })
                })
                .collect();
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
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

async fn build_rejected(
    pool: &SqlitePool,
    merchant_id: &str,
    txid: &str,
    zatoshis: u64,
    previously_verified: bool,
    reason: &str,
    protocol: &str,
) -> VerifyResponse {
    log_verification(
        pool,
        merchant_id,
        txid,
        zatoshis,
        "rejected",
        Some(reason),
        protocol,
    )
    .await;
    VerifyResponse {
        valid: false,
        received_zec: zatoshis as f64 / 100_000_000.0,
        received_zatoshis: zatoshis,
        previously_verified,
        reason: Some(reason.to_string()),
    }
}

async fn get_existing_verified(
    pool: &SqlitePool,
    merchant_id: &str,
    txid: &str,
    protocol: &str,
) -> Option<u64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT amount_zatoshis FROM x402_verifications
         WHERE merchant_id = ? AND txid = ? AND protocol = ? AND status = 'verified'
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(merchant_id)
    .bind(txid)
    .bind(protocol)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|amount| amount.max(0) as u64)
}

async fn log_verification(
    pool: &SqlitePool,
    merchant_id: &str,
    txid: &str,
    amount_zatoshis: u64,
    status: &str,
    reason: Option<&str>,
    protocol: &str,
) {
    let id = Uuid::new_v4().to_string();
    let amount_zec = amount_zatoshis as f64 / 100_000_000.0;
    let insert_sql = if status == "verified" {
        "INSERT OR IGNORE INTO x402_verifications (id, merchant_id, txid, amount_zatoshis, amount_zec, status, reason, protocol)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    } else {
        "INSERT INTO x402_verifications (id, merchant_id, txid, amount_zatoshis, amount_zec, status, reason, protocol)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    };

    let result = sqlx::query(insert_sql)
        .bind(&id)
        .bind(merchant_id)
        .bind(txid)
        .bind(amount_zatoshis as i64)
        .bind(amount_zec)
        .bind(status)
        .bind(reason)
        .bind(protocol)
        .execute(pool)
        .await;

    if let Err(e) = result {
        tracing::warn!(error = %e, "Failed to log x402 verification");
    }
}

fn merchant_billing_blocked(status: &str) -> bool {
    status == "past_due" || status == "suspended"
}

fn zec_to_zatoshis(amount_zec: f64) -> Option<u64> {
    if !amount_zec.is_finite() || amount_zec < 0.0 {
        return None;
    }

    let scaled = (amount_zec * 100_000_000.0).round();
    if scaled < 1.0 || scaled > 2_100_000_000_000_000.0 {
        return None;
    }

    Some(scaled as u64)
}

// ---------------------------------------------------------------------------
// x402 V2 spec-compliant facilitator endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct PaymentRequirementsV2 {
    scheme: Option<String>,
    network: Option<String>,
    amount: Option<String>,
    #[serde(rename = "payTo")]
    pay_to: Option<String>,
    #[serde(rename = "maxTimeoutSeconds")]
    max_timeout_seconds: Option<u64>,
    asset: Option<String>,
    extra: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ZcashPayload {
    txid: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PaymentPayloadV2 {
    #[serde(rename = "x402Version")]
    x402_version: Option<u32>,
    resource: Option<serde_json::Value>,
    accepted: Option<serde_json::Value>,
    payload: Option<ZcashPayload>,
    extensions: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VerifyRequestV2 {
    #[serde(rename = "x402Version")]
    pub x402_version: Option<u32>,
    #[serde(rename = "paymentPayload")]
    pub payment_payload: PaymentPayloadV2,
    #[serde(rename = "paymentRequirements")]
    pub payment_requirements: PaymentRequirementsV2,
}

/// Optional session config sent alongside settle to auto-create a session.
#[derive(Debug, Deserialize, Serialize)]
struct SettleSessionConfig {
    #[serde(rename = "costPerRequest")]
    cost_per_request: Option<i64>,
    #[serde(rename = "refundAddress")]
    refund_address: Option<String>,
}

/// Extended settle request: standard x402 V2 fields + optional session bridge.
#[derive(Debug, Deserialize, Serialize)]
pub struct SettleRequestV2 {
    #[serde(rename = "x402Version")]
    pub x402_version: Option<u32>,
    #[serde(rename = "paymentPayload")]
    pub payment_payload: PaymentPayloadV2,
    #[serde(rename = "paymentRequirements")]
    pub payment_requirements: PaymentRequirementsV2,
    /// If present, auto-create a session after successful settlement.
    session: Option<SettleSessionConfig>,
    /// RFC Idempotency-Key — if repeated, returns cached response.
    #[serde(rename = "idempotencyKey")]
    idempotency_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct VerifyResponseV2 {
    #[serde(rename = "isValid")]
    is_valid: bool,
    #[serde(rename = "invalidReason", skip_serializing_if = "Option::is_none")]
    invalid_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payer: Option<String>,
}

/// Session info returned when auto-session is created on settle.
#[derive(Debug, Serialize)]
struct SessionInfo {
    token: String,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "balanceRemaining")]
    balance_remaining: i64,
    #[serde(rename = "costPerRequest")]
    cost_per_request: i64,
    #[serde(rename = "expiresAt")]
    expires_at: String,
}

#[derive(Debug, Serialize)]
struct SettleResponseV2 {
    success: bool,
    transaction: String,
    network: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payer: Option<String>,
    #[serde(rename = "errorReason", skip_serializing_if = "Option::is_none")]
    error_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionInfo>,
}

// ---------------------------------------------------------------------------
// RFC 9457 Problem Details (https://www.rfc-editor.org/rfc/rfc9457)
// ---------------------------------------------------------------------------

fn problem_details(status: u16, error_type: &str, title: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "type": format!("https://cipherpay.app/errors/{}", error_type),
        "title": title,
        "status": status,
        "detail": detail,
    })
}

/// Extract and validate the common fields from a V2 request body.
/// Works for both VerifyRequestV2 and SettleRequestV2 via trait.
fn parse_v2_fields(
    payload: &PaymentPayloadV2,
    requirements: &PaymentRequirementsV2,
) -> Result<(String, u64, String), HttpResponse> {
    if payload.x402_version != Some(2)
        || requirements.scheme.as_deref() != Some("exact")
        || requirements.asset.as_deref() != Some("ZEC")
        || requirements.pay_to.as_deref().unwrap_or("").is_empty()
        || requirements.max_timeout_seconds.unwrap_or(0) == 0
    {
        return Err(HttpResponse::BadRequest().json(problem_details(
            400,
            "invalid-payment-requirements",
            "Invalid Payment Requirements",
            "Require V2 exact ZEC payment, payTo and positive timeout",
        )));
    }
    if let Some(accepted) = &payload.accepted {
        let expected = serde_json::to_value(requirements).unwrap_or_default();
        for field in ["scheme", "network", "amount", "payTo", "asset"] {
            if accepted.get(field) != expected.get(field) {
                return Err(HttpResponse::BadRequest().json(problem_details(
                    400,
                    "requirements-mismatch",
                    "Requirements Mismatch",
                    "Accepted payment differs from resource requirements",
                )));
            }
        }
    }
    let txid = payload
        .payload
        .as_ref()
        .and_then(|p| p.txid.as_deref())
        .unwrap_or("");

    if txid.len() != 64 || !txid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(HttpResponse::BadRequest()
            .insert_header(("Content-Type", "application/problem+json"))
            .json(problem_details(
                400,
                "invalid-payload",
                "Invalid Payment Payload",
                "txid must be a 64-character hex string",
            )));
    }

    let amount_str = requirements.amount.as_deref().unwrap_or("0");

    let expected_zatoshis: u64 = amount_str.parse().map_err(|_| {
        HttpResponse::BadRequest()
            .insert_header(("Content-Type", "application/problem+json"))
            .json(problem_details(
                400,
                "invalid-payment-requirements",
                "Invalid Payment Requirements",
                "amount must be a valid integer string (zatoshis)",
            ))
    })?;

    if expected_zatoshis == 0 || expected_zatoshis > 2_100_000_000_000_000 {
        return Err(HttpResponse::BadRequest()
            .insert_header(("Content-Type", "application/problem+json"))
            .json(problem_details(
                400,
                "invalid-payment-requirements",
                "Invalid Payment Requirements",
                "amount must be greater than zero",
            )));
    }

    let network = requirements
        .network
        .as_deref()
        .unwrap_or("zcash:mainnet")
        .to_string();

    Ok((txid.to_ascii_lowercase(), expected_zatoshis, network))
}

fn parse_v2_request(body: &VerifyRequestV2) -> Result<(String, u64, String), HttpResponse> {
    parse_v2_fields(&body.payment_payload, &body.payment_requirements)
}

fn parse_settle_request(body: &SettleRequestV2) -> Result<(String, u64, String), HttpResponse> {
    parse_v2_fields(&body.payment_payload, &body.payment_requirements)
}

/// Core verification logic shared by verify_v2 and settle_v2.
async fn verify_core_v2(
    pool: &SqlitePool,
    config: &Config,
    http_client: &reqwest::Client,
    txid: &str,
    expected_zatoshis: u64,
    network: &str,
    pay_to: &str,
    api_key: &str,
) -> Result<(bool, Option<String>, u64), HttpResponse> {
    let merchant = match merchants::authenticate(pool, api_key, &config.encryption_key).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return Err(HttpResponse::Unauthorized()
                .insert_header(("Content-Type", "application/problem+json"))
                .json(problem_details(
                    401,
                    "unauthorized",
                    "Unauthorized",
                    "Invalid API key",
                )));
        }
        Err(e) => {
            tracing::error!(error = %e, "x402 v2 auth error");
            return Err(HttpResponse::InternalServerError()
                .insert_header(("Content-Type", "application/problem+json"))
                .json(problem_details(
                    500,
                    "internal-error",
                    "Internal Error",
                    "Unexpected verification error",
                )));
        }
    };

    let expected_network = if config.is_testnet() {
        "zcash:testnet"
    } else {
        "zcash:mainnet"
    };
    if network != expected_network || pay_to != merchant.payment_address {
        return Err(HttpResponse::BadRequest().json(problem_details(
            400,
            "requirements-mismatch",
            "Requirements Mismatch",
            "Network and payTo must match this merchant",
        )));
    }
    let receiver = crate::invoices::matching::orchard_receiver(pay_to).ok_or_else(|| {
        HttpResponse::BadRequest().json(problem_details(
            400,
            "invalid-pay-to",
            "Invalid Recipient",
            "Orchard receiver required",
        ))
    })?;
    if config.fee_enabled() {
        if let Ok(status) = crate::billing::get_merchant_billing_status(pool, &merchant.id).await {
            if merchant_billing_blocked(&status) {
                return Err(HttpResponse::PaymentRequired()
                    .insert_header(("Content-Type", "application/problem+json"))
                    .json(problem_details(
                        402,
                        "merchant-billing-blocked",
                        "Merchant Billing Blocked",
                        "Merchant account has outstanding fees",
                    )));
            }
        }
    }

    let protocol = "x402";
    let min_acceptable = expected_zatoshis;

    if !crate::scanner::blocks::check_tx_confirmed(http_client, &config.cipherscan_api_url, txid)
        .await
        .unwrap_or(false)
    {
        return Ok((false, Some("payment_not_confirmed".to_string()), 0));
    }
    let raw_hex = match mempool::fetch_raw_tx(http_client, &config.cipherscan_api_url, txid).await {
        Ok(hex) => hex,
        Err(e) => {
            tracing::warn!(txid = %txid, error = %e, "x402 v2: failed to fetch raw tx");
            log_verification(
                pool,
                &merchant.id,
                txid,
                0,
                "rejected",
                Some("Transaction not found"),
                protocol,
            )
            .await;
            return Ok((false, Some("invalid_transaction_state".to_string()), 0));
        }
    };

    let outputs = match decrypt::try_decrypt_all_outputs_ivk(&raw_hex, &merchant.ufvk) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(txid = %txid, error = %e, "x402 v2: decryption error");
            log_verification(
                pool,
                &merchant.id,
                txid,
                0,
                "rejected",
                Some("Decryption failed"),
                protocol,
            )
            .await;
            return Ok((false, Some("invalid_transaction_state".to_string()), 0));
        }
    };

    if outputs.is_empty() {
        log_verification(
            pool,
            &merchant.id,
            txid,
            0,
            "rejected",
            Some("No outputs addressed to this merchant"),
            protocol,
        )
        .await;
        return Ok((false, Some("invalid_payload".to_string()), 0));
    }

    let total_zatoshis: u64 = outputs
        .iter()
        .filter(|o| hex::encode(o.recipient_raw) == receiver)
        .map(|o| o.amount_zatoshis)
        .sum();

    if total_zatoshis >= min_acceptable {
        log_verification(
            pool,
            &merchant.id,
            txid,
            total_zatoshis,
            "verified",
            None,
            protocol,
        )
        .await;
        Ok((true, None, total_zatoshis))
    } else {
        log_verification(
            pool,
            &merchant.id,
            txid,
            total_zatoshis,
            "rejected",
            Some("Insufficient amount"),
            protocol,
        )
        .await;
        Ok((
            false,
            Some("insufficient_funds".to_string()),
            total_zatoshis,
        ))
    }
}

/// POST /api/x402/v2/verify — x402 V2 spec-compliant verify endpoint.
pub async fn verify_v2(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    http_client: web::Data<reqwest::Client>,
    body: web::Json<VerifyRequestV2>,
) -> HttpResponse {
    let api_key = match extract_api_key(&req) {
        Some(k) => k,
        None => {
            return HttpResponse::Unauthorized()
                .insert_header(("Content-Type", "application/problem+json"))
                .json(problem_details(
                    401,
                    "unauthorized",
                    "Unauthorized",
                    "Missing or invalid Authorization header",
                ));
        }
    };

    let (txid, expected_zatoshis, network) = match parse_v2_request(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match verify_core_v2(
        pool.get_ref(),
        &config,
        &http_client,
        &txid,
        expected_zatoshis,
        &network,
        body.payment_requirements.pay_to.as_deref().unwrap_or(""),
        &api_key,
    )
    .await
    {
        Ok((is_valid, invalid_reason, _)) => HttpResponse::Ok().json(VerifyResponseV2 {
            is_valid,
            invalid_reason,
            payer: None,
        }),
        Err(resp) => resp,
    }
}

/// POST /api/x402/v2/settle — x402 V2 spec-compliant settle endpoint.
///
/// For Zcash, settlement is implicit (tx already on-chain), so this verifies
/// the transaction and records it. Supports:
/// - **Idempotency-Key** header for safe retries
/// - **Auto-session**: optional `session` field to create a session in one step
pub async fn settle_v2(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    http_client: web::Data<reqwest::Client>,
    body: web::Json<SettleRequestV2>,
) -> HttpResponse {
    let api_key = match extract_api_key(&req) {
        Some(k) => k,
        None => {
            return HttpResponse::Unauthorized()
                .insert_header(("Content-Type", "application/problem+json"))
                .json(problem_details(
                    401,
                    "unauthorized",
                    "Unauthorized",
                    "Missing or invalid Authorization header",
                ));
        }
    };

    let merchant =
        match merchants::authenticate(pool.get_ref(), &api_key, &config.encryption_key).await {
            Ok(Some(m)) => m,
            _ => {
                return HttpResponse::Unauthorized().json(problem_details(
                    401,
                    "unauthorized",
                    "Unauthorized",
                    "Invalid API key",
                ))
            }
        };
    let key = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| body.idempotency_key.clone());
    if key.as_ref().is_some_and(|k| k.is_empty() || k.len() > 128) {
        return HttpResponse::BadRequest().json(problem_details(
            400,
            "invalid-idempotency-key",
            "Invalid Key",
            "Use 1-128 characters",
        ));
    }
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&*body).unwrap_or_default(),
    ));
    if let Some(key) = &key {
        match cached_settlement(
            pool.get_ref(),
            &merchant.id,
            key,
            &digest,
            &config.encryption_key,
        )
        .await
        {
            Ok(Some(response)) => {
                return HttpResponse::Ok()
                    .insert_header(("Idempotency-Replayed", "true"))
                    .json(response)
            }
            Ok(None) => {}
            Err(_) => {
                return HttpResponse::Conflict().json(problem_details(
                    409,
                    "idempotency-conflict",
                    "Idempotency Conflict",
                    "Key belongs to another request or cannot be recovered",
                ))
            }
        }
    }
    let (txid, amount, network) = match parse_settle_request(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match verify_core_v2(
        pool.get_ref(),
        &config,
        &http_client,
        &txid,
        amount,
        &network,
        body.payment_requirements.pay_to.as_deref().unwrap_or(""),
        &api_key,
    )
    .await
    {
        Ok((true, _, total)) => {
            match commit_settlement(
                pool.get_ref(),
                &merchant.id,
                &txid,
                amount,
                total,
                &network,
                body.session.as_ref(),
                key.as_deref(),
                &digest,
                &config.encryption_key,
            )
            .await
            {
                Ok(Some(response)) => HttpResponse::Ok().json(response),
                Ok(None) => HttpResponse::Conflict().json(problem_details(
                    409,
                    "payment-replayed",
                    "Payment Replayed",
                    "This transaction has already been consumed",
                )),
                Err(e) => {
                    tracing::error!(error = %e, "Settlement commit failed");
                    HttpResponse::InternalServerError().finish()
                }
            }
        }
        Ok((false, reason, _)) => HttpResponse::Ok().json(SettleResponseV2 {
            success: false,
            transaction: String::new(),
            network,
            payer: None,
            error_reason: reason,
            amount: None,
            session: None,
        }),
        Err(r) => r,
    }
}

async fn cached_settlement(
    pool: &SqlitePool,
    merchant: &str,
    key: &str,
    digest: &str,
    encryption_key: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let row = sqlx::query_as::<_, (String, String)>("SELECT request_hash, response_json FROM x402_idempotency_v2 WHERE merchant_id = ? AND idempotency_key = ? AND created_at >= strftime('%Y-%m-%dT%H:%M:%SZ','now','-24 hours')")
        .bind(merchant).bind(key).fetch_optional(pool).await?;
    if let Some((hash, encrypted)) = row {
        anyhow::ensure!(hash == digest, "Idempotency request mismatch");
        return Ok(Some(serde_json::from_str(&crate::crypto::decrypt(
            &encrypted,
            encryption_key,
        )?)?));
    }
    Ok(None)
}

async fn commit_settlement(
    pool: &SqlitePool,
    merchant: &str,
    txid: &str,
    price: u64,
    total: u64,
    network: &str,
    session: Option<&SettleSessionConfig>,
    key: Option<&str>,
    digest: &str,
    encryption_key: &str,
) -> anyhow::Result<Option<SettleResponseV2>> {
    let mut tx = pool.begin().await?;
    if !crate::sessions::consume_payment(&mut tx, txid, "x402").await? {
        return Ok(None);
    }
    // Only the surplus funds a session; the current fulfilled request is already paid for.
    let remaining = total.saturating_sub(price) as i64;
    let session_info = if let Some(cfg) = session {
        let cost = cfg.cost_per_request.unwrap_or(1000);
        anyhow::ensure!(cost > 0, "Invalid session cost");
        if remaining >= cost {
            let s = crate::sessions::insert_session(
                &mut tx,
                merchant,
                txid,
                remaining,
                cfg.refund_address.as_deref(),
                Some(cost),
            )
            .await?;
            Some(SessionInfo {
                token: s.bearer_token,
                session_id: s.id,
                balance_remaining: s.balance_remaining,
                cost_per_request: s.cost_per_request,
                expires_at: s.expires_at,
            })
        } else {
            None
        }
    } else {
        None
    };
    let response = SettleResponseV2 {
        success: true,
        transaction: txid.to_string(),
        network: network.to_string(),
        payer: None,
        error_reason: None,
        amount: Some(total.to_string()),
        session: session_info,
    };
    if let Some(key) = key {
        let encrypted = crate::crypto::encrypt(&serde_json::to_string(&response)?, encryption_key)?;
        sqlx::query("DELETE FROM x402_idempotency_v2 WHERE merchant_id = ? AND idempotency_key = ? AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ','now','-24 hours')")
            .bind(merchant).bind(key).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO x402_idempotency_v2 (merchant_id, idempotency_key, request_hash, response_json) VALUES (?, ?, ?, ?)")
            .bind(merchant).bind(key).bind(digest).bind(encrypted).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Some(response))
}

/// GET /api/x402/supported — x402 V2 spec-compliant discovery endpoint.
pub async fn supported(config: web::Data<Config>) -> HttpResponse {
    let network = if config.is_testnet() {
        "zcash:testnet"
    } else {
        "zcash:mainnet"
    };
    HttpResponse::Ok()
        .insert_header(("Access-Control-Allow-Origin", "*"))
        .insert_header(("Cache-Control", "public, max-age=3600"))
        .json(serde_json::json!({
            "kinds": [{
                "x402Version": 2,
                "scheme": "exact",
                "network": network,
            }],
            "extensions": [],
            "signers": {},
        }))
}

#[cfg(test)]
mod repair_tests {
    use super::*;
    #[tokio::test]
    async fn settlement_consumption_and_cache_are_tenant_bound() {
        let pool = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&pool, "a").await;
        crate::repair_tests::merchant(&pool, "b").await;
        let encryption_key = "11".repeat(32);
        let config = SettleSessionConfig {
            cost_per_request: Some(1000),
            refund_address: None,
        };
        let first = commit_settlement(
            &pool,
            "a",
            "tx",
            1000,
            5000,
            "zcash:mainnet",
            Some(&config),
            Some("key"),
            "digest",
            &encryption_key,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.session.unwrap().balance_remaining, 4000);
        assert!(commit_settlement(
            &pool,
            "a",
            "tx",
            1000,
            5000,
            "zcash:mainnet",
            None,
            None,
            "digest",
            &encryption_key
        )
        .await
        .unwrap()
        .is_none());
        assert!(
            cached_settlement(&pool, "b", "key", "digest", &encryption_key)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            cached_settlement(&pool, "a", "key", "different-body", &encryption_key)
                .await
                .is_err()
        );
        assert!(
            cached_settlement(&pool, "a", "key", "digest", &encryption_key)
                .await
                .unwrap()
                .is_some()
        );
        let stored: String = sqlx::query_scalar("SELECT response_json FROM x402_idempotency_v2")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!stored.contains("cps_"));
        assert!(
            crate::sessions::create_session(&pool, "a", "tx", 5000, None)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn failed_settlement_rolls_back_consumption() {
        let pool = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&pool, "a").await;
        // Invalid encryption config fails cache persistence; the payment must remain spendable.
        assert!(commit_settlement(
            &pool,
            "a",
            "tx",
            1000,
            1000,
            "zcash:mainnet",
            None,
            Some("key"),
            "digest",
            ""
        )
        .await
        .is_err());
        assert!(!crate::sessions::txid_already_used(&pool, "tx").await);
    }
    #[actix_web::test]
    async fn unauthenticated_settle_cannot_read_cached_credentials() {
        let pool = crate::repair_tests::pool().await;
        sqlx::query("INSERT INTO x402_idempotency_v2 VALUES ('a','known-key','hash','sensitive','2099-01-01T00:00:00Z')").execute(&pool).await.unwrap();
        let config = Config::from_env().unwrap();
        let app = actix_web::test::init_service(
            actix_web::App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(reqwest::Client::new()))
                .route("/settle", web::post().to(settle_v2)),
        )
        .await;
        let req=actix_web::test::TestRequest::post().uri("/settle").insert_header(("Authorization","Bearer invalid"))
            .insert_header(("Idempotency-Key","known-key"))
            .set_json(serde_json::json!({"x402Version":2,"paymentPayload":{"x402Version":2,"payload":{"txid":"a".repeat(64)}},"paymentRequirements":{"scheme":"exact","network":"zcash:mainnet","asset":"ZEC","amount":"1000","payTo":"address","maxTimeoutSeconds":300}})).to_request();
        let response = actix_web::test::call_service(&app, req).await;
        assert_eq!(response.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    }
}
