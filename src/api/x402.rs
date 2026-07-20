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
        let min_acceptable = (expected_zatoshis as f64 * SLIPPAGE_TOLERANCE) as u64;

        if received_zatoshis >= min_acceptable {
            return HttpResponse::Ok().json(VerifyResponse {
                valid: true,
                received_zec: received_zatoshis as f64 / 100_000_000.0,
                received_zatoshis,
                previously_verified: true,
                reason: None,
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

    let total_zatoshis: u64 = outputs.iter().map(|o| o.amount_zatoshis).sum();
    let total_zec = total_zatoshis as f64 / 100_000_000.0;
    let expected_zatoshis = match zec_to_zatoshis(body.expected_amount_zec) {
        Some(amount) => amount,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "expected_amount_zec must be representable in zatoshis"
            }));
        }
    };
    let min_acceptable = (expected_zatoshis as f64 * SLIPPAGE_TOLERANCE) as u64;

    if total_zatoshis >= min_acceptable {
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
    if scaled < 0.0 || scaled > u64::MAX as f64 {
        return None;
    }

    Some(scaled as u64)
}

// ---------------------------------------------------------------------------
// x402 V2 spec-compliant facilitator endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
struct ZcashPayload {
    txid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PaymentPayloadV2 {
    #[serde(rename = "x402Version")]
    x402_version: Option<u32>,
    resource: Option<serde_json::Value>,
    accepted: Option<serde_json::Value>,
    payload: Option<ZcashPayload>,
    extensions: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequestV2 {
    #[serde(rename = "x402Version")]
    pub x402_version: Option<u32>,
    #[serde(rename = "paymentPayload")]
    pub payment_payload: PaymentPayloadV2,
    #[serde(rename = "paymentRequirements")]
    pub payment_requirements: PaymentRequirementsV2,
}

/// Optional session config sent alongside settle to auto-create a session.
#[derive(Debug, Deserialize)]
struct SettleSessionConfig {
    #[serde(rename = "costPerRequest")]
    cost_per_request: Option<i64>,
    #[serde(rename = "refundAddress")]
    refund_address: Option<String>,
}

/// Extended settle request: standard x402 V2 fields + optional session bridge.
#[derive(Debug, Deserialize)]
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

fn problem_details(
    status: u16,
    error_type: &str,
    title: &str,
    detail: &str,
) -> serde_json::Value {
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

    if expected_zatoshis == 0 {
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

    Ok((txid.to_string(), expected_zatoshis, network))
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
    api_key: &str,
) -> Result<(bool, Option<String>, u64), HttpResponse> {
    let merchant = match merchants::authenticate(pool, api_key, &config.encryption_key).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return Err(HttpResponse::Unauthorized()
                .insert_header(("Content-Type", "application/problem+json"))
                .json(problem_details(401, "unauthorized", "Unauthorized", "Invalid API key")));
        }
        Err(e) => {
            tracing::error!(error = %e, "x402 v2 auth error");
            return Err(HttpResponse::InternalServerError()
                .insert_header(("Content-Type", "application/problem+json"))
                .json(problem_details(500, "internal-error", "Internal Error", "Unexpected verification error")));
        }
    };

    if config.fee_enabled() {
        if let Ok(status) =
            crate::billing::get_merchant_billing_status(pool, &merchant.id).await
        {
            if merchant_billing_blocked(&status) {
                return Err(HttpResponse::PaymentRequired()
                    .insert_header(("Content-Type", "application/problem+json"))
                    .json(problem_details(402, "merchant-billing-blocked", "Merchant Billing Blocked", "Merchant account has outstanding fees")));
            }
        }
    }

    let protocol = "x402";
    let min_acceptable = (expected_zatoshis as f64 * SLIPPAGE_TOLERANCE) as u64;

    if let Some(received_zatoshis) =
        get_existing_verified(pool, &merchant.id, txid, protocol).await
    {
        if received_zatoshis >= min_acceptable {
            return Ok((true, None, received_zatoshis));
        } else {
            return Ok((
                false,
                Some("insufficient_funds".to_string()),
                received_zatoshis,
            ));
        }
    }

    let raw_hex =
        match mempool::fetch_raw_tx(http_client, &config.cipherscan_api_url, txid).await {
            Ok(hex) => hex,
            Err(e) => {
                tracing::warn!(txid = %txid, error = %e, "x402 v2: failed to fetch raw tx");
                log_verification(pool, &merchant.id, txid, 0, "rejected", Some("Transaction not found"), protocol).await;
                return Ok((false, Some("invalid_transaction_state".to_string()), 0));
            }
        };

    let outputs = match decrypt::try_decrypt_all_outputs_ivk(&raw_hex, &merchant.ufvk) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(txid = %txid, error = %e, "x402 v2: decryption error");
            log_verification(pool, &merchant.id, txid, 0, "rejected", Some("Decryption failed"), protocol).await;
            return Ok((false, Some("invalid_transaction_state".to_string()), 0));
        }
    };

    if outputs.is_empty() {
        log_verification(pool, &merchant.id, txid, 0, "rejected", Some("No outputs addressed to this merchant"), protocol).await;
        return Ok((false, Some("invalid_payload".to_string()), 0));
    }

    let total_zatoshis: u64 = outputs.iter().map(|o| o.amount_zatoshis).sum();

    if total_zatoshis >= min_acceptable {
        log_verification(pool, &merchant.id, txid, total_zatoshis, "verified", None, protocol).await;
        Ok((true, None, total_zatoshis))
    } else {
        log_verification(pool, &merchant.id, txid, total_zatoshis, "rejected", Some("Insufficient amount"), protocol).await;
        Ok((false, Some("insufficient_funds".to_string()), total_zatoshis))
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
                .json(problem_details(401, "unauthorized", "Unauthorized", "Missing or invalid Authorization header"));
        }
    };

    let (txid, expected_zatoshis, network) = match parse_v2_request(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match verify_core_v2(pool.get_ref(), &config, &http_client, &txid, expected_zatoshis, &network, &api_key).await {
        Ok((is_valid, invalid_reason, _)) => {
            HttpResponse::Ok().json(VerifyResponseV2 {
                is_valid,
                invalid_reason,
                payer: None,
            })
        }
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
                .json(problem_details(401, "unauthorized", "Unauthorized", "Missing or invalid Authorization header"));
        }
    };

    // Idempotency-Key: check header first, fall back to body field
    let idempotency_key = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| body.idempotency_key.clone());

    if let Some(ref key) = idempotency_key {
        if let Some(cached) = get_idempotent_response(pool.get_ref(), key).await {
            return HttpResponse::Ok()
                .insert_header(("Idempotency-Replayed", "true"))
                .json(cached);
        }
    }

    let (txid, expected_zatoshis, network) = match parse_settle_request(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match verify_core_v2(pool.get_ref(), &config, &http_client, &txid, expected_zatoshis, &network, &api_key).await {
        Ok((true, _, total_zatoshis)) => {
            // Auto-session: if requested and payment verified, create a session
            let session_info = if let Some(ref session_cfg) = body.session {
                match try_auto_session(pool.get_ref(), &api_key, &config, &txid, total_zatoshis, session_cfg).await {
                    Ok(Some(info)) => Some(info),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, "Auto-session creation failed (settle still succeeds)");
                        None
                    }
                }
            } else {
                None
            };

            let response = SettleResponseV2 {
                success: true,
                transaction: txid,
                network,
                payer: None,
                error_reason: None,
                amount: Some(total_zatoshis.to_string()),
                session: session_info,
            };

            if let Some(ref key) = idempotency_key {
                store_idempotent_response(pool.get_ref(), key, &response).await;
            }

            HttpResponse::Ok().json(response)
        }
        Ok((false, reason, _)) => {
            HttpResponse::Ok().json(SettleResponseV2 {
                success: false,
                transaction: String::new(),
                network,
                payer: None,
                error_reason: reason,
                amount: None,
                session: None,
            })
        }
        Err(resp) => resp,
    }
}

/// Try to auto-create a session from a settled payment.
async fn try_auto_session(
    pool: &SqlitePool,
    api_key: &str,
    config: &Config,
    txid: &str,
    total_zatoshis: u64,
    session_cfg: &SettleSessionConfig,
) -> Result<Option<SessionInfo>, anyhow::Error> {
    if crate::sessions::txid_already_used(pool, txid).await {
        return Ok(None);
    }

    let merchant = match merchants::authenticate(pool, api_key, &config.encryption_key).await? {
        Some(m) => m,
        None => return Ok(None),
    };

    let cost_per_request = session_cfg.cost_per_request.unwrap_or(1_000);
    if cost_per_request <= 0 || (total_zatoshis as i64) < cost_per_request {
        return Ok(None);
    }

    let session = crate::sessions::create_session_with_cost(
        pool,
        &merchant.id,
        txid,
        total_zatoshis as i64,
        session_cfg.refund_address.as_deref(),
        session_cfg.cost_per_request,
    )
    .await?;

    Ok(Some(SessionInfo {
        token: session.bearer_token,
        session_id: session.id,
        balance_remaining: session.balance_remaining,
        cost_per_request: session.cost_per_request,
        expires_at: session.expires_at,
    }))
}

// ---------------------------------------------------------------------------
// Idempotency-Key storage (x402_idempotency table)
// ---------------------------------------------------------------------------

async fn get_idempotent_response(pool: &SqlitePool, key: &str) -> Option<serde_json::Value> {
    let row = sqlx::query_scalar::<_, String>(
        "SELECT response_json FROM x402_idempotency
         WHERE idempotency_key = ? AND created_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-24 hours')"
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .ok()?;

    row.and_then(|json_str| serde_json::from_str(&json_str).ok())
}

async fn store_idempotent_response(pool: &SqlitePool, key: &str, response: &SettleResponseV2) {
    let json_str = match serde_json::to_string(response) {
        Ok(s) => s,
        Err(_) => return,
    };

    let result = sqlx::query(
        "INSERT OR IGNORE INTO x402_idempotency (idempotency_key, response_json) VALUES (?, ?)"
    )
    .bind(key)
    .bind(&json_str)
    .execute(pool)
    .await;

    if let Err(e) = result {
        tracing::warn!(error = %e, "Failed to store idempotent response");
    }
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
