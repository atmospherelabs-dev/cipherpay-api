use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::scanner::{decrypt, mempool};

const SESSION_MEMO_PREFIX: &str = "zipher:session:";

#[derive(Debug, Deserialize)]
pub struct OpenRequest {
    pub txid: String,
    /// Required for memo-based flow; optional when using session_request_id.
    pub merchant_id: Option<String>,
    pub refund_address: Option<String>,
    /// If provided, uses address-based verification (no memo needed).
    pub session_request_id: Option<String>,
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

    // Resolve merchant: either from session_request_id or merchant_id
    let (merchant_id, expected_address, expected_receiver_hex, diversifier_index) = if let Some(
        ref sr_id,
    ) =
        body.session_request_id
    {
        match crate::sessions::get_session_request(pool.get_ref(), sr_id).await {
            Ok(Some(sr)) => (
                sr.merchant_id,
                Some(sr.deposit_address),
                None,
                Some(sr.diversifier_index),
            ),
            Ok(None) => {
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Session request not found, already used, or expired"
                }));
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to lookup session request");
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }));
            }
        }
    } else if let Some(ref mid) = body.merchant_id {
        tracing::warn!(merchant_id = %mid, "Deprecated: memo-based session opening. Use POST /api/sessions/prepare + session_request_id instead.");
        (mid.clone(), None, None, None)
    } else {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "session_request_id is required. Use POST /api/sessions/prepare to get one."
        }));
    };

    let merchant = match crate::merchants::get_merchant_by_id(
        pool.get_ref(),
        &merchant_id,
        &config.encryption_key,
    )
    .await
    {
        Ok(Some(m)) => m,
        _ => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Merchant not found"
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

    let expected_receiver_hex = match diversifier_index {
        Some(index) => match crate::addresses::derive_invoice_address(&merchant.ufvk, index) {
            Ok(derived) => Some(derived.orchard_receiver_hex),
            Err(e) => {
                tracing::error!(error = %e, merchant_id = %merchant.id, "Failed to derive expected session receiver");
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }));
            }
        },
        None => expected_receiver_hex,
    };

    let raw_hex =
        match mempool::fetch_raw_tx(&http_client, &config.cipherscan_api_url, &body.txid).await {
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

    // Two verification paths: address-based (no memo) or memo-based (legacy)
    let total_zatoshis: i64 = if let Some(ref addr) = expected_address {
        let Some(expected_receiver_hex) = expected_receiver_hex.as_ref() else {
            tracing::error!(merchant_id = %merchant.id, "Missing expected receiver for session request");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        };

        let total = sum_outputs_for_receiver(&outputs, expected_receiver_hex);
        if total == 0 {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "No outputs to the expected deposit address"
            }));
        }
        tracing::info!(address = %addr, total_zatoshis = total, "Address-based session deposit verified");
        total
    } else {
        // Memo-based: filter by session memo
        let expected_memo = format!("{}{}", SESSION_MEMO_PREFIX, merchant_id);
        let session_outputs: Vec<_> = outputs
            .iter()
            .filter(|o| o.memo.trim() == expected_memo)
            .collect();

        if session_outputs.is_empty() {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "No output with matching session memo found in transaction",
            }));
        }
        session_outputs
            .iter()
            .map(|o| o.amount_zatoshis as i64)
            .sum()
    };

    if total_zatoshis < 10_000 {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Deposit too small — minimum 10,000 zatoshis (0.0001 ZEC)"
        }));
    }

    // Mark session request as used (if address-based)
    if let Some(ref sr_id) = body.session_request_id {
        if let Err(e) = crate::sessions::mark_session_request_used(pool.get_ref(), sr_id).await {
            tracing::warn!(error = %e, "Failed to mark session request as used");
        }
    }

    match crate::sessions::create_session(
        pool.get_ref(),
        &merchant_id,
        &body.txid,
        total_zatoshis,
        body.refund_address.as_deref(),
    )
    .await
    {
        Ok(session) => HttpResponse::Created().json(serde_json::json!({
            "session_id": session.id,
            "bearer_token": session.bearer_token,
            "balance": session.balance_remaining,
            "expires_at": session.expires_at,
            "cost_per_request": session.cost_per_request,
        })),
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
    let session = crate::sessions::get_session(pool, session_id)
        .await
        .map_err(|_| {
            HttpResponse::InternalServerError().json(serde_json::json!({"error": "Internal error"}))
        })?
        .ok_or_else(|| {
            HttpResponse::NotFound().json(serde_json::json!({"error": "Session not found"}))
        })?;

    // Try dashboard session auth
    if let Some(merchant) =
        crate::api::auth::resolve_session(req, &web::Data::new(pool.clone())).await
    {
        if merchant.id == session.merchant_id {
            return Ok(());
        }
    }

    // Try API key auth
    if let Some(auth_header) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            let key = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str).trim();
            let enc_key = req
                .app_data::<web::Data<Config>>()
                .map(|c| c.encryption_key.clone())
                .unwrap_or_default();
            if let Ok(Some(merchant)) = crate::merchants::authenticate(pool, key, &enc_key).await {
                if merchant.id == session.merchant_id {
                    return Ok(());
                }
            }
        }
    }

    Err(HttpResponse::Unauthorized()
        .json(serde_json::json!({"error": "Not authorized for this session"})))
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
        Ok(Some(summary)) => HttpResponse::Ok().json(serde_json::json!({
            "session_id": summary.session_id,
            "requests_made": summary.requests_made,
            "balance_used": summary.balance_used,
            "balance_remaining": summary.balance_remaining,
            "status": summary.status,
        })),
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

pub async fn history(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
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
            let items: Vec<_> = sessions
                .iter()
                .map(|s| {
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
                        if s.balance_remaining > 0
                            && (s.status == "closed"
                                || s.status == "depleted"
                                || s.status == "expired")
                        {
                            let refund_zec = s.balance_remaining as f64 / 100_000_000.0;
                            obj.as_object_mut().unwrap().insert(
                                "refund".to_string(),
                                serde_json::json!({
                                    "address": addr,
                                    "amount_zatoshis": s.balance_remaining,
                                    "amount_zec": refund_zec,
                                }),
                            );
                        }
                    }
                    obj
                })
                .collect();

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

/// Prepare a session deposit: generates a unique payment address (no memo needed).
pub async fn prepare(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<PrepareRequest>,
) -> HttpResponse {
    let merchant =
        match resolve_prepare_merchant(&req, pool.get_ref(), &config, &body.merchant_id).await {
            Ok(m) => m,
            Err(resp) => return resp,
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

    match crate::sessions::create_session_request(pool.get_ref(), &merchant.id, &merchant.ufvk)
        .await
    {
        Ok(req) => HttpResponse::Ok().json(serde_json::json!({
            "session_request_id": req.id,
            "deposit_address": req.deposit_address,
            "merchant_id": merchant.id,
            "min_deposit_zatoshis": 10_000,
            "expires_in_seconds": 1800,
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to prepare session");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to prepare session deposit"
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PrepareRequest {
    pub merchant_id: String,
}

/// Deduct a variable amount from a session (for streaming metering).
pub async fn deduct(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<DeductRequest>,
) -> HttpResponse {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("cps_"));

    let token = match token {
        Some(t) => t,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Bearer token required"
            }));
        }
    };

    if body.amount_zatoshis <= 0 {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "amount_zatoshis must be positive"
        }));
    }

    if let Err(resp) = enforce_session_billing(&pool, &config, &token).await {
        return resp;
    }

    match crate::sessions::deduct(pool.get_ref(), &token, body.amount_zatoshis).await {
        Ok(Some(session)) => HttpResponse::Ok()
            .insert_header(("X-Session-Balance", session.balance_remaining.to_string()))
            .json(serde_json::json!({
                "valid": true,
                "session_id": session.id,
                "balance_remaining": session.balance_remaining,
                "deducted": body.amount_zatoshis,
            })),
        Ok(None) => HttpResponse::Ok().json(serde_json::json!({
            "valid": false,
            "reason": "Insufficient balance, session expired, or depleted"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Session deduction error");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DeductRequest {
    pub amount_zatoshis: i64,
}

pub async fn validate(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
) -> HttpResponse {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("cps_"));

    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Bearer token required — use Authorization: Bearer cps_... header"
            }));
        }
    };

    if let Err(resp) = enforce_session_billing(&pool, &config, &token).await {
        return resp;
    }

    match crate::sessions::validate_and_deduct(pool.get_ref(), &token).await {
        Ok(Some(session)) => HttpResponse::Ok()
            .insert_header(("X-Session-Balance", session.balance_remaining.to_string()))
            .json(serde_json::json!({
                "valid": true,
                "session_id": session.id,
                "balance_remaining": session.balance_remaining,
                "requests_made": session.requests_made,
            })),
        Ok(None) => HttpResponse::Ok().json(serde_json::json!({
            "valid": false,
            "reason": "Session not found, expired, or depleted"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Session validation error");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

async fn resolve_prepare_merchant(
    req: &HttpRequest,
    pool: &SqlitePool,
    config: &Config,
    merchant_id: &str,
) -> Result<crate::merchants::Merchant, HttpResponse> {
    if let Some(merchant) =
        crate::api::auth::resolve_session(req, &web::Data::new(pool.clone())).await
    {
        if merchant.id == merchant_id {
            return Ok(merchant);
        }
        return Err(HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Not authorized for this merchant"
        })));
    }

    let Some(auth_header) = req.headers().get("Authorization") else {
        return Err(HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Authentication required"
        })));
    };

    let Ok(auth_str) = auth_header.to_str() else {
        return Err(HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Authentication required"
        })));
    };

    let key = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str).trim();
    match crate::merchants::authenticate(pool, key, &config.encryption_key).await {
        Ok(Some(merchant)) if merchant.id == merchant_id => Ok(merchant),
        Ok(Some(_)) => Err(HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Not authorized for this merchant"
        }))),
        Ok(None) => Err(HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Authentication required"
        }))),
        Err(e) => {
            tracing::error!(error = %e, "Failed to authenticate session prepare request");
            Err(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            })))
        }
    }
}

async fn enforce_session_billing(
    pool: &web::Data<SqlitePool>,
    config: &web::Data<Config>,
    token: &str,
) -> Result<(), HttpResponse> {
    if !config.fee_enabled() {
        return Ok(());
    }

    let Some(session) = crate::sessions::get_session_by_token(pool.get_ref(), token)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to resolve session for billing enforcement");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        })?
    else {
        return Ok(());
    };

    if let Ok(status) =
        crate::billing::get_merchant_billing_status(pool.get_ref(), &session.merchant_id).await
    {
        if merchant_billing_blocked(&status) {
            return Err(HttpResponse::Ok().json(serde_json::json!({
                "valid": false,
                "reason": "Merchant account has outstanding fees",
                "billing_status": status,
            })));
        }
    }

    Ok(())
}

fn merchant_billing_blocked(status: &str) -> bool {
    status == "past_due" || status == "suspended"
}

fn sum_outputs_for_receiver(
    outputs: &[crate::scanner::decrypt::DecryptedOutput],
    expected_receiver_hex: &str,
) -> i64 {
    outputs
        .iter()
        .filter(|o| hex::encode(o.recipient_raw) == expected_receiver_hex)
        .map(|o| o.amount_zatoshis as i64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::sum_outputs_for_receiver;
    use crate::scanner::decrypt::DecryptedOutput;

    #[test]
    fn sums_only_outputs_for_expected_receiver() {
        let matching_receiver = [7u8; 43];
        let other_receiver = [9u8; 43];
        let expected_receiver_hex = hex::encode(matching_receiver);
        let outputs = vec![
            DecryptedOutput {
                memo: String::new(),
                amount_zec: 0.1,
                amount_zatoshis: 10_000,
                recipient_raw: matching_receiver,
            },
            DecryptedOutput {
                memo: String::new(),
                amount_zec: 0.2,
                amount_zatoshis: 20_000,
                recipient_raw: other_receiver,
            },
            DecryptedOutput {
                memo: String::new(),
                amount_zec: 0.3,
                amount_zatoshis: 30_000,
                recipient_raw: matching_receiver,
            },
        ];

        assert_eq!(
            sum_outputs_for_receiver(&outputs, &expected_receiver_hex),
            40_000
        );
    }
}
