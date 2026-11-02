use actix_web::cookie::{Cookie, SameSite};
use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;
use crate::merchants;
use crate::validation;

const SESSION_COOKIE: &str = "__Host-cpay_session";
const SESSION_COOKIE_LEGACY: &str = "cpay_session";
const POS_SESSION_COOKIE: &str = "__Host-cpay_pos";
const POS_SESSION_COOKIE_LEGACY: &str = "cpay_pos";
const SESSION_HOURS: i64 = 24;

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub token: String,
}

/// POST /api/auth/session -- exchange dashboard token for an HttpOnly session cookie
pub async fn create_session(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<CreateSessionRequest>,
) -> HttpResponse {
    let merchant = match merchants::authenticate_dashboard(
        pool.get_ref(),
        &body.token,
        &config.encryption_key,
    )
    .await
    {
        Ok(Some(m)) => m,
        Ok(None) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid dashboard token"
            }));
        }
        Err(e) => {
            tracing::error!(error = %e, "Session auth error");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };

    let session_id = Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + Duration::hours(SESSION_HOURS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    if let Err(e) =
        sqlx::query("INSERT INTO sessions (id, merchant_id, expires_at) VALUES (?, ?, ?)")
            .bind(&session_id)
            .bind(&merchant.id)
            .bind(&expires_at)
            .execute(pool.get_ref())
            .await
    {
        tracing::error!(error = %e, "Failed to create session");
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Failed to create session"
        }));
    }

    // Track last token login timestamp
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let _ = sqlx::query("UPDATE merchants SET last_token_login_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&merchant.id)
        .execute(pool.get_ref())
        .await;

    let cookie = build_session_cookie(&session_id, &config, false);

    HttpResponse::Ok().cookie(cookie).json(serde_json::json!({
        "merchant_id": merchant.id,
        "payment_address": merchant.payment_address,
    }))
}

/// POST /api/auth/logout -- clear the session cookie and delete the session
pub async fn logout(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
) -> HttpResponse {
    if let Some(session_id) = extract_session_id(&req) {
        let _ = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(&session_id)
            .execute(pool.get_ref())
            .await;
    }

    let cookie = build_session_cookie("", &config, true);

    HttpResponse::Ok()
        .cookie(cookie)
        .json(serde_json::json!({ "status": "logged_out" }))
}

/// GET /api/merchants/me -- get current merchant info from session cookie
pub async fn me(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let stats = get_merchant_stats(pool.get_ref(), &merchant.id).await;

    let masked_secret = if merchant.webhook_secret.len() > 12 {
        format!("{}...", &merchant.webhook_secret[..12])
    } else if merchant.webhook_secret.is_empty() {
        String::new()
    } else {
        "***".to_string()
    };

    let masked_email = merchant.recovery_email.as_deref().map(|email| {
        if let Some(at) = email.find('@') {
            let local = &email[..at];
            let domain = &email[at..];
            let visible = if local.len() <= 2 { local.len() } else { 2 };
            format!(
                "{}{}{}",
                &local[..visible],
                "*".repeat(local.len().saturating_sub(visible)),
                domain
            )
        } else {
            "***".to_string()
        }
    });

    let has_luma_key: bool =
        sqlx::query_scalar::<_, Option<String>>("SELECT luma_api_key FROM merchants WHERE id = ?")
            .bind(&merchant.id)
            .fetch_optional(pool.get_ref())
            .await
            .ok()
            .flatten()
            .flatten()
            .map(|k| !k.is_empty())
            .unwrap_or(false);

    let crosschain_enabled: bool = sqlx::query_scalar("SELECT crosschain_enabled FROM merchants WHERE id=?").bind(&merchant.id).fetch_one(pool.get_ref()).await.unwrap_or(false);
    let payment_email_notifications: bool = match sqlx::query_scalar(
        "SELECT payment_email_notifications FROM merchants WHERE id = ?",
    )
    .bind(&merchant.id)
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(enabled) => enabled,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load payment email preference");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Could not load payment settings"}));
        }
    };
    let has_passkeys = super::passkey::has_passkeys(pool.get_ref(), &merchant.id).await;

    let last_token_login: Option<String> =
        sqlx::query_scalar("SELECT last_token_login_at FROM merchants WHERE id = ?")
            .bind(&merchant.id)
            .fetch_optional(pool.get_ref())
            .await
            .ok()
            .flatten();

    HttpResponse::Ok().json(serde_json::json!({
        "id": merchant.id,
        "name": merchant.name,
        "payment_address": merchant.payment_address,
        "webhook_url": merchant.webhook_url,
        "webhook_secret_preview": masked_secret,
        "has_recovery_email": merchant.recovery_email.is_some(),
        "recovery_email_preview": masked_email,
        "payment_email_notifications": payment_email_notifications,
        "created_at": merchant.created_at,
        "stats": stats,
        "has_luma_key": has_luma_key,
        "crosschain_enabled": crosschain_enabled,
        "has_passkeys": has_passkeys,
        "last_token_login_at": last_token_login,
    }))
}

/// GET /api/merchants/me/invoices -- list invoices for the authenticated merchant
pub async fn my_invoices(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match resolve_full_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let rows = sqlx::query_as::<_, crate::invoices::Invoice>(
        "SELECT id, merchant_id, memo_code, product_id, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
         amount, price_id, subscription_id,
         payment_address, zcash_uri,
         NULL AS merchant_name,
         refund_address, status, detected_txid, detected_at,
         confirmed_at, refunded_at, refund_txid, expires_at, purge_after, created_at,
         orchard_receiver_hex, diversifier_index,
         price_zatoshis, received_zatoshis,
         payment_link_id, is_donation, campaign_counted,
         confirmed_rate, confirmed_fiat_amount
         FROM invoices WHERE merchant_id = ?
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(invoices) => {
            let crosschain_ids: std::collections::HashSet<String> = match sqlx::query_scalar::<_, String>(
                "SELECT i.id FROM invoices i WHERE i.merchant_id = ? AND EXISTS (SELECT 1 FROM crosschain_attempts c WHERE c.invoice_id = i.id) ORDER BY i.created_at DESC LIMIT 100",
            ).bind(&merchant.id).fetch_all(pool.get_ref()).await {
                Ok(ids) => ids.into_iter().collect(),
                Err(e) => {
                    tracing::error!(error = %e, "Failed to load invoice payment methods");
                    return HttpResponse::InternalServerError().json(serde_json::json!({"error":"Internal error"}));
                }
            };
            let event_product_ids: std::collections::HashSet<String> =
                sqlx::query_scalar::<_, String>(
                    "SELECT product_id FROM events WHERE merchant_id = ?",
                )
                .bind(&merchant.id)
                .fetch_all(pool.get_ref())
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            let luma_product_ids: std::collections::HashSet<String> =
                sqlx::query_scalar::<_, String>(
                    "SELECT product_id FROM events WHERE merchant_id = ? AND luma_event_id IS NOT NULL",
                )
                .bind(&merchant.id)
                .fetch_all(pool.get_ref())
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            let price_ids: Vec<String> = invoices
                .iter()
                .filter_map(|inv| inv.price_id.clone())
                .collect();
            let price_labels: std::collections::HashMap<String, String> = if price_ids.is_empty() {
                std::collections::HashMap::new()
            } else {
                use sqlx::Row;
                let placeholders = price_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let query_str = format!(
                    "SELECT id, label FROM prices WHERE id IN ({}) AND label IS NOT NULL",
                    placeholders
                );
                let mut q = sqlx::query(&query_str);
                for pid in &price_ids {
                    q = q.bind(pid);
                }
                q.fetch_all(pool.get_ref())
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| (r.get::<String, _>("id"), r.get::<String, _>("label")))
                    .collect()
            };

            let access: std::collections::HashMap<String,String> = sqlx::query_as::<_,(String,String)>("SELECT a.invoice_id,a.token FROM checkout_access a JOIN invoices i ON i.id=a.invoice_id WHERE i.merchant_id=?")
                .bind(&merchant.id).fetch_all(pool.get_ref()).await.unwrap_or_default().into_iter().collect();
            let enriched: Vec<serde_json::Value> = invoices
                .iter()
                .map(|inv| {
                    let mut val = serde_json::to_value(inv).unwrap_or(serde_json::json!({}));
                    if let serde_json::Value::Object(ref mut map) = val {
                        if let Some(token)=access.get(&inv.id) { map.insert("checkout_token".into(),serde_json::json!(token)); }
                        let is_event = inv
                            .product_id
                            .as_ref()
                            .map(|pid| event_product_ids.contains(pid))
                            .unwrap_or(false);
                        let is_luma = inv
                            .product_id
                            .as_ref()
                            .map(|pid| luma_product_ids.contains(pid))
                            .unwrap_or(false);
                        map.insert("is_event".into(), serde_json::json!(is_event));
                        map.insert("is_luma".into(), serde_json::json!(is_luma));
                        map.insert(
                            "is_crosschain".into(),
                            serde_json::json!(crosschain_ids.contains(&inv.id)),
                        );
                        let label = inv.price_id.as_ref().and_then(|pid| price_labels.get(pid));
                        map.insert("price_label".into(), serde_json::json!(label));
                    }
                    val
                })
                .collect();

            HttpResponse::Ok().json(enriched)
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list merchant invoices");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

/// GET /api/merchants/me/webhooks -- list webhook deliveries for the authenticated merchant
pub async fn my_webhooks(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    query: web::Query<MyWebhookQuery>,
) -> HttpResponse {
    let merchant = match resolve_full_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let limit = query.limit.unwrap_or(50).min(200) as i64;
    let offset = query.offset.unwrap_or(0) as i64;

    let (count_sql, list_sql) = if let Some(ref _status) = query.status {
        (
            "SELECT COUNT(*) FROM webhook_deliveries WHERE merchant_id = ? AND status = ?".to_string(),
            "SELECT id, invoice_id, event_type, status, response_status, response_error, attempts, created_at, last_attempt_at
             FROM webhook_deliveries WHERE merchant_id = ? AND status = ? ORDER BY created_at DESC LIMIT ? OFFSET ?".to_string(),
        )
    } else {
        (
            "SELECT COUNT(*) FROM webhook_deliveries WHERE merchant_id = ?".to_string(),
            "SELECT id, invoice_id, event_type, status, response_status, response_error, attempts, created_at, last_attempt_at
             FROM webhook_deliveries WHERE merchant_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?".to_string(),
        )
    };

    let total: i64 = if let Some(ref status) = query.status {
        sqlx::query_scalar::<_, i64>(&count_sql)
            .bind(&merchant.id)
            .bind(status)
            .fetch_one(pool.get_ref())
            .await
            .unwrap_or(0)
    } else {
        sqlx::query_scalar::<_, i64>(&count_sql)
            .bind(&merchant.id)
            .fetch_one(pool.get_ref())
            .await
            .unwrap_or(0)
    };

    let rows: Vec<(
        String,
        String,
        Option<String>,
        String,
        Option<i32>,
        Option<String>,
        i32,
        String,
        Option<String>,
    )> = if let Some(ref status) = query.status {
        sqlx::query_as(&list_sql)
            .bind(&merchant.id)
            .bind(status)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool.get_ref())
            .await
            .unwrap_or_default()
    } else {
        sqlx::query_as(&list_sql)
            .bind(&merchant.id)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool.get_ref())
            .await
            .unwrap_or_default()
    };

    let deliveries: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.0,
                "invoice_id": r.1,
                "event_type": r.2,
                "status": r.3,
                "response_status": r.4,
                "response_error": r.5,
                "attempts": r.6,
                "created_at": r.7,
                "last_attempt_at": r.8,
            })
        })
        .collect();

    HttpResponse::Ok().json(serde_json::json!({
        "deliveries": deliveries,
        "total": total,
    }))
}

#[derive(serde::Deserialize)]
pub struct MyWebhookQuery {
    pub status: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// POST /api/merchants/me/webhooks/{id}/retry — manually retry a failed webhook delivery
pub async fn retry_webhook(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match resolve_full_session(&req, &pool).await {
        Some(m) => m,
        None => return not_authenticated_response(),
    };

    let config = match req.app_data::<web::Data<crate::config::Config>>() {
        Some(c) => c,
        None => {
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Server config unavailable"}));
        }
    };

    let delivery_id = path.into_inner();

    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT wd.url, wd.payload, m.webhook_secret
         FROM webhook_deliveries wd
         JOIN merchants m ON wd.merchant_id = m.id
         WHERE wd.id = ? AND wd.merchant_id = ? AND wd.status = 'failed'",
    )
    .bind(&delivery_id)
    .bind(&merchant.id)
    .fetch_optional(pool.get_ref())
    .await;

    let (url, payload, raw_secret) = match row {
        Ok(Some(r)) => r,
        Ok(None) => {
            return HttpResponse::NotFound()
                .json(serde_json::json!({"error": "Delivery not found or not failed"}));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch delivery for retry");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let secret = match crate::crypto::decrypt_webhook_secret(&raw_secret, &config.encryption_key) {
        Ok(s) => s,
        Err(_) => {
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Failed to decrypt webhook secret"}));
        }
    };

    if let Err(reason) = crate::validation::resolve_and_check_host(&url) {
        return HttpResponse::BadRequest()
            .json(serde_json::json!({"error": format!("Blocked: {reason}")}));
    }

    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut body: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(_) => {
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Corrupt payload"}));
        }
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "timestamp".to_string(),
            serde_json::Value::String(ts.clone()),
        );
    }

    let updated_payload = body.to_string();
    let signature = crate::webhooks::sign_payload_public(&secret, &ts, &updated_payload);

    let http = match req.app_data::<web::Data<reqwest::Client>>() {
        Some(c) => c.clone(),
        None => {
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "HTTP client unavailable"}));
        }
    };
    let result = http
        .post(&url)
        .header("X-CipherPay-Signature", &signature)
        .header("X-CipherPay-Timestamp", &ts)
        .header("X-CipherPay-Delivery-Id", &delivery_id)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let status_code = resp.status().as_u16() as i32;
            let _ = sqlx::query(
                "UPDATE webhook_deliveries SET status = 'delivered', response_status = ?, response_error = NULL, last_attempt_at = ? WHERE id = ?",
            )
            .bind(status_code)
            .bind(&ts)
            .bind(&delivery_id)
            .execute(pool.get_ref())
            .await;
            tracing::info!(delivery_id = %delivery_id, "Manual webhook retry delivered");
            HttpResponse::Ok().json(serde_json::json!({"status": "delivered"}))
        }
        Ok(resp) => {
            let status_code = resp.status().as_u16() as i32;
            let error_text = format!("HTTP {}", resp.status());
            let _ = sqlx::query(
                "UPDATE webhook_deliveries SET response_status = ?, response_error = ?, last_attempt_at = ? WHERE id = ?",
            )
            .bind(status_code)
            .bind(&error_text)
            .bind(&ts)
            .bind(&delivery_id)
            .execute(pool.get_ref())
            .await;
            HttpResponse::Ok().json(serde_json::json!({"status": "failed", "error": error_text}))
        }
        Err(e) => {
            let error_text = e.to_string();
            let _ = sqlx::query(
                "UPDATE webhook_deliveries SET response_status = 0, response_error = ?, last_attempt_at = ? WHERE id = ?",
            )
            .bind(&error_text)
            .bind(&ts)
            .bind(&delivery_id)
            .execute(pool.get_ref())
            .await;
            HttpResponse::Ok().json(serde_json::json!({"status": "failed", "error": error_text}))
        }
    }
}

/// Extract the session ID from the session cookie (__Host- prefixed or legacy)
pub fn extract_session_id(req: &HttpRequest) -> Option<String> {
    req.cookie(SESSION_COOKIE)
        .or_else(|| req.cookie(SESSION_COOKIE_LEGACY))
        .map(|c| c.value().to_string())
        .filter(|v| !v.is_empty())
}

pub fn not_authenticated_response() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({
        "error": "Not authenticated"
    }))
}

pub fn invalid_api_key_response() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({
        "error": "Invalid API key"
    }))
}

/// Resolve a merchant from the session cookie
pub async fn resolve_session(req: &HttpRequest, pool: &SqlitePool) -> Option<merchants::Merchant> {
    let config = req.app_data::<web::Data<crate::config::Config>>()?;
    if let Some(session_id) = extract_session_id(req) {
        if let Ok(Some(m)) =
            merchants::get_by_session(pool, &session_id, &config.encryption_key).await
        {
            return Some(m);
        }
    }
    if let Some(pos_id) = extract_pos_session_id(req) {
        if let Ok(Some(m)) = merchants::get_by_session(pool, &pos_id, &config.encryption_key).await
        {
            return Some(m);
        }
    }
    None
}

/// Resolve a merchant from session cookie, rejecting POS-scoped sessions.
/// Use for account-management endpoints that must not be accessible via PIN.
pub async fn resolve_full_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Option<merchants::Merchant> {
    let session_id = extract_session_id(req)?;
    let config = req.app_data::<web::Data<crate::config::Config>>()?;
    let (merchant, pos_scoped) =
        merchants::get_by_session_with_scope(pool, &session_id, &config.encryption_key)
            .await
            .ok()??;
    if pos_scoped {
        return None;
    }
    Some(merchant)
}

/// Resolve a merchant from either API key (Bearer token) or session cookie,
/// alongside which credential type matched. Used by scope-aware helpers.
pub async fn resolve_with_kind(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Option<(merchants::Merchant, merchants::KeyKind)> {
    let config = req.app_data::<web::Data<crate::config::Config>>()?;

    if let Some(auth) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth.to_str() {
            let key = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str).trim();
            if key.starts_with("cpay_sk_")
                || key.starts_with("cpay_rk_")
                || key.starts_with("cpay_")
            {
                return merchants::authenticate_with_kind(pool, key, &config.encryption_key)
                    .await
                    .ok()?;
            }
        }
    }

    let m = resolve_session(req, pool).await?;
    Some((m, merchants::KeyKind::Session))
}

/// Resolve a merchant from either API key (Bearer token) or session cookie.
/// Backwards-compatible wrapper that drops the credential kind.
pub async fn resolve_merchant_or_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Option<merchants::Merchant> {
    resolve_with_kind(req, pool).await.map(|(m, _)| m)
}

pub async fn require_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Result<merchants::Merchant, HttpResponse> {
    resolve_session(req, pool)
        .await
        .ok_or_else(not_authenticated_response)
}

/// Like require_session but rejects POS-scoped sessions.
pub async fn require_full_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Result<merchants::Merchant, HttpResponse> {
    resolve_full_session(req, pool)
        .await
        .ok_or_else(not_authenticated_response)
}

pub async fn require_merchant_or_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Result<merchants::Merchant, HttpResponse> {
    resolve_merchant_or_session(req, pool)
        .await
        .ok_or_else(not_authenticated_response)
}

/// Reject restricted API keys AND POS-scoped sessions. Use for endpoints that
/// mutate account-level configuration: PATCH /me, key/secret rotation, billing
/// actions, account deletion, key management.
pub async fn require_full_or_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Result<merchants::Merchant, HttpResponse> {
    match resolve_with_kind(req, pool).await {
        Some((m, kind)) => {
            if matches!(kind, merchants::KeyKind::Restricted) {
                Err(restricted_key_forbidden_response())
            } else if matches!(kind, merchants::KeyKind::Session) {
                resolve_full_session(req, pool)
                    .await
                    .ok_or_else(pos_session_forbidden_response)
            } else {
                Ok(m)
            }
        }
        None => Err(not_authenticated_response()),
    }
}

fn pos_session_forbidden_response() -> HttpResponse {
    HttpResponse::Forbidden().json(serde_json::json!({
        "error": "POS sessions cannot access account management endpoints.",
        "code": "pos_session_forbidden",
    }))
}

pub fn restricted_key_forbidden_response() -> HttpResponse {
    HttpResponse::Forbidden().json(serde_json::json!({
        "error": "This endpoint requires a full-access API key. Restricted keys cannot perform account-management actions.",
        "code": "restricted_key_forbidden",
    }))
}

pub async fn require_api_key_or_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Result<merchants::Merchant, HttpResponse> {
    if let Some(merchant) = resolve_session(req, pool).await {
        return Ok(merchant);
    }

    let Some(auth_header) = req.headers().get("Authorization") else {
        return Err(not_authenticated_response());
    };
    let Ok(auth_str) = auth_header.to_str() else {
        return Err(not_authenticated_response());
    };

    let key = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str).trim();
    let enc_key = req
        .app_data::<web::Data<crate::config::Config>>()
        .map(|c| c.encryption_key.clone())
        .unwrap_or_default();

    match crate::merchants::authenticate(pool, key, &enc_key).await {
        Ok(Some(merchant)) => Ok(merchant),
        _ => Err(invalid_api_key_response()),
    }
}

pub fn build_session_cookie<'a>(value: &str, config: &Config, clear: bool) -> Cookie<'a> {
    let has_domain = config.cookie_domain.is_some();
    let is_deployed = has_domain
        || config
            .frontend_url
            .as_deref()
            .map_or(false, |u| u.starts_with("https"));
    let use_secure = !config.is_testnet() || is_deployed;

    // __Host- cookies require the Secure flag; fall back to legacy name on local HTTP
    let cookie_name = if has_domain {
        SESSION_COOKIE_LEGACY
    } else if use_secure {
        SESSION_COOKIE
    } else {
        SESSION_COOKIE_LEGACY
    };

    let mut builder = Cookie::build(cookie_name, value.to_string())
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax);

    if use_secure {
        builder = builder.secure(true);
        if let Some(ref domain) = config.cookie_domain {
            builder = builder.domain(domain.clone());
        }
    }

    if clear {
        builder = builder.max_age(actix_web::cookie::time::Duration::ZERO);
    } else {
        builder = builder.max_age(actix_web::cookie::time::Duration::hours(SESSION_HOURS));
    }

    builder.finish()
}

pub fn build_pos_cookie<'a>(value: &str, config: &Config, clear: bool) -> Cookie<'a> {
    let has_domain = config.cookie_domain.is_some();
    let is_deployed = has_domain
        || config
            .frontend_url
            .as_deref()
            .map_or(false, |u| u.starts_with("https"));
    let use_secure = !config.is_testnet() || is_deployed;

    let cookie_name = if has_domain {
        POS_SESSION_COOKIE_LEGACY
    } else if use_secure {
        POS_SESSION_COOKIE
    } else {
        POS_SESSION_COOKIE_LEGACY
    };

    let mut builder = Cookie::build(cookie_name, value.to_string())
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax);

    if use_secure {
        builder = builder.secure(true);
        if let Some(ref domain) = config.cookie_domain {
            builder = builder.domain(domain.clone());
        }
    }

    if clear {
        builder = builder.max_age(actix_web::cookie::time::Duration::ZERO);
    } else {
        builder = builder.max_age(actix_web::cookie::time::Duration::hours(4));
    }

    builder.finish()
}

pub fn extract_pos_session_id(req: &HttpRequest) -> Option<String> {
    req.cookie(POS_SESSION_COOKIE)
        .or_else(|| req.cookie(POS_SESSION_COOKIE_LEGACY))
        .map(|c| c.value().to_string())
        .filter(|v| !v.is_empty())
}

#[derive(Debug, Deserialize)]
pub struct UpdateMerchantRequest {
    pub name: Option<String>,
    pub webhook_url: Option<String>,
    pub recovery_email: Option<String>,
    pub payment_email_notifications: Option<bool>,
    pub luma_api_key: Option<String>,
    pub crosschain_enabled: Option<bool>,
}

/// PATCH /api/merchants/me -- update merchant settings and payment email preference.
///
/// Payment address is intentionally NOT editable after registration.
/// It is cryptographically tied to the UFVK used for trial decryption.
/// Allowing changes would either:
///   - Break payment detection (new address from different wallet)
///   - Enable session-hijack fund diversion (attacker changes to their address)
/// Merchants who need a new address must re-register with a new UFVK.
pub async fn update_me(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<UpdateMerchantRequest>,
) -> HttpResponse {
    let merchant = match resolve_full_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    if let Err(e) = validate_update(&body, config.is_testnet()) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    let encrypt = |value: &str| -> anyhow::Result<String> {
        if config.encryption_key.is_empty() {
            Ok(value.to_string())
        } else {
            crate::crypto::encrypt(value, &config.encryption_key)
        }
    };
    let encrypted_email = match body
        .recovery_email
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(encrypt)
        .transpose()
    {
        Ok(email) => email,
        Err(e) => {
            tracing::error!(error = %e, "Failed to encrypt recovery email");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };
    let encrypted_luma_key = match body
        .luma_api_key
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(encrypt)
        .transpose()
    {
        Ok(key) => key,
        Err(e) => {
            tracing::error!(error = %e, "Failed to encrypt Luma API key");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(error = %e, "Failed to begin merchant settings update");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Could not save payment settings"}));
        }
    };

    // Read inside the transaction so a concurrent email removal cannot leave
    // notifications enabled without a recipient. Validate before any writes.
    if body.payment_email_notifications == Some(true) {
        let effective_email = if let Some(email) = body.recovery_email.as_deref() {
            Some(email.to_string())
        } else {
            let stored_email: Option<String> =
                match sqlx::query_scalar("SELECT recovery_email FROM merchants WHERE id = ?")
                    .bind(&merchant.id)
                    .fetch_one(&mut *tx)
                    .await
                {
                    Ok(email) => email,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to load notification recipient");
                        return HttpResponse::InternalServerError()
                            .json(serde_json::json!({"error": "Could not save payment settings"}));
                    }
                };
            match stored_email
                .map(|email| crate::crypto::decrypt_email(&email, &config.encryption_key))
                .transpose()
            {
                Ok(email) => email,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to decrypt notification recipient");
                    return HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "Could not save payment settings"}));
                }
            }
        };
        let Some(email) = effective_email.as_deref().filter(|email| !email.is_empty()) else {
            return HttpResponse::BadRequest().json(
                validation::ValidationError::invalid(
                    "payment_email_notifications",
                    "add a recovery email before enabling payment notifications",
                )
                .to_json(),
            );
        };
        if let Err(e) = validation::validate_email_format("recovery_email", email) {
            return HttpResponse::BadRequest().json(e.to_json());
        }
    }

    let result: Result<(), sqlx::Error> = async {
        if let Some(enabled) = body.crosschain_enabled {
            sqlx::query("UPDATE merchants SET crosschain_enabled = ? WHERE id = ?")
                .bind(enabled).bind(&merchant.id).execute(&mut *tx).await?;
        }
        if let Some(ref name) = body.name {
            sqlx::query("UPDATE merchants SET name = ? WHERE id = ?")
                .bind(name).bind(&merchant.id).execute(&mut *tx).await?;
        }
        if let Some(ref url) = body.webhook_url {
            sqlx::query("UPDATE merchants SET webhook_url = ? WHERE id = ?")
                .bind(if url.is_empty() { None } else { Some(url.as_str()) })
                .bind(&merchant.id).execute(&mut *tx).await?;
        }
        if let Some(ref email) = body.recovery_email {
            let hash = if email.is_empty() { None } else {
                Some(crate::crypto::blind_index(email, &config.encryption_key))
            };
            sqlx::query(
                "UPDATE merchants SET recovery_email = ?, recovery_email_hash = ?,
                 payment_email_notifications = CASE WHEN ? THEN 0 ELSE payment_email_notifications END
                 WHERE id = ?",
            )
            .bind(&encrypted_email).bind(hash).bind(email.is_empty())
            .bind(&merchant.id).execute(&mut *tx).await?;
        }
        if let Some(enabled) = body.payment_email_notifications {
            sqlx::query("UPDATE merchants SET payment_email_notifications = ? WHERE id = ?")
                .bind(enabled).bind(&merchant.id).execute(&mut *tx).await?;
        }
        if body.luma_api_key.is_some() {
            sqlx::query("UPDATE merchants SET luma_api_key = ? WHERE id = ?")
                .bind(&encrypted_luma_key).bind(&merchant.id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }.await;
    if let Err(e) = result {
        tracing::error!(error = %e, merchant_id = %merchant.id, "Failed to update merchant settings");
        return HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "Could not save payment settings"}));
    }
    tracing::info!(merchant_id = %merchant.id, "Merchant settings updated");

    HttpResponse::Ok().json(serde_json::json!({ "status": "updated" }))
}

/// POST /api/merchants/me/regenerate-api-key
pub async fn regenerate_api_key(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match resolve_full_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({ "error": "Not authenticated" }))
        }
    };

    match merchants::regenerate_api_key(pool.get_ref(), &merchant.id).await {
        Ok(new_key) => HttpResponse::Ok().json(serde_json::json!({ "api_key": new_key })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to regenerate API key");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({ "error": "Failed to regenerate" }))
        }
    }
}

/// POST /api/merchants/me/regenerate-dashboard-token
pub async fn regenerate_dashboard_token(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match resolve_full_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({ "error": "Not authenticated" }))
        }
    };

    match merchants::regenerate_dashboard_token(pool.get_ref(), &merchant.id).await {
        Ok(new_token) => {
            HttpResponse::Ok().json(serde_json::json!({ "dashboard_token": new_token }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to regenerate dashboard token");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({ "error": "Failed to regenerate" }))
        }
    }
}

/// POST /api/merchants/me/regenerate-webhook-secret
pub async fn regenerate_webhook_secret(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
) -> HttpResponse {
    let merchant = match resolve_full_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({ "error": "Not authenticated" }))
        }
    };

    match merchants::regenerate_webhook_secret(pool.get_ref(), &merchant.id, &config.encryption_key)
        .await
    {
        Ok(new_secret) => {
            HttpResponse::Ok().json(serde_json::json!({ "webhook_secret": new_secret }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to regenerate webhook secret");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({ "error": "Failed to regenerate" }))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RecoverRequest {
    pub email: String,
}

/// POST /api/auth/recover -- request a recovery email.
/// Uses constant-time response delay to prevent email enumeration via timing.
pub async fn recover(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<RecoverRequest>,
) -> HttpResponse {
    if !config.smtp_configured() {
        return HttpResponse::ServiceUnavailable().json(serde_json::json!({
            "error": "Email recovery is not configured on this instance"
        }));
    }

    if let Err(e) = validation::validate_email_format("email", &body.email) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    let start = std::time::Instant::now();

    let result: Result<(), ()> = async {
        let merchant =
            match merchants::find_by_email(pool.get_ref(), &body.email, &config.encryption_key)
                .await
            {
                Ok(Some(m)) => m,
                _ => return Err(()),
            };

        let token = merchants::create_recovery_token(pool.get_ref(), &merchant.id)
            .await
            .map_err(|e| tracing::error!(error = %e, "Failed to create recovery token"))?;

        crate::email::send_recovery_email(&config, &body.email, &token)
            .await
            .map_err(|e| tracing::error!(error = %e, "Failed to send recovery email"))?;

        Ok(())
    }
    .await;

    // Constant-time: always wait at least 2 seconds to prevent timing side-channel
    let elapsed = start.elapsed();
    let min_duration = std::time::Duration::from_secs(2);
    if elapsed < min_duration {
        tokio::time::sleep(min_duration - elapsed).await;
    }

    if result.is_err() {
        // Same response whether email doesn't exist or sending failed
    }

    HttpResponse::Ok().json(serde_json::json!({
        "message": "If an account with this email exists, a recovery link has been sent"
    }))
}

#[derive(Debug, Deserialize)]
pub struct RecoverConfirmRequest {
    pub token: String,
}

/// POST /api/auth/recover/confirm -- exchange recovery token for new dashboard token
pub async fn recover_confirm(
    pool: web::Data<SqlitePool>,
    body: web::Json<RecoverConfirmRequest>,
) -> HttpResponse {
    match merchants::confirm_recovery_token(pool.get_ref(), &body.token).await {
        Ok(Some(new_dashboard_token)) => HttpResponse::Ok().json(serde_json::json!({
            "dashboard_token": new_dashboard_token,
            "message": "Account recovered. Save your new dashboard token."
        })),
        Ok(None) => {
            tracing::warn!("Recovery confirm failed: token not found or expired");
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Invalid or expired recovery token"
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Recovery confirmation failed");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Recovery failed"
            }))
        }
    }
}

async fn get_merchant_stats(pool: &SqlitePool, merchant_id: &str) -> serde_json::Value {
    let row = sqlx::query_as::<_, (i64, i64, f64)>(
        "SELECT
            COUNT(*) as total,
            COUNT(CASE WHEN status = 'confirmed' THEN 1 END) as confirmed,
            COALESCE(SUM(CASE WHEN status = 'confirmed' THEN price_zec ELSE 0.0 END), 0.0) as total_zec
         FROM invoices WHERE merchant_id = ?"
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or((0, 0, 0.0));

    serde_json::json!({
        "payment_methods": crate::crosschain::fees::payment_methods(pool, merchant_id).await.ok(),
        "total_invoices": row.0,
        "confirmed": row.1,
        "total_zec": row.2,
    })
}

fn validate_update(
    req: &UpdateMerchantRequest,
    is_testnet: bool,
) -> Result<(), validation::ValidationError> {
    if let Some(ref name) = req.name {
        validation::validate_length("name", name, 100)?;
    }
    if let Some(ref url) = req.webhook_url {
        if !url.is_empty() {
            validation::validate_webhook_url("webhook_url", url, is_testnet)?;
        }
    }
    if let Some(ref email) = req.recovery_email {
        if !email.is_empty() {
            validation::validate_email_format("recovery_email", email)?;
        }
    }
    Ok(())
}

/// Read the existing integration secret without rotating it as a setup side effect.
pub async fn webhook_config(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
) -> HttpResponse {
    let merchant = match require_full_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let secret = match crate::crypto::decrypt_webhook_secret(
        &merchant.webhook_secret,
        &config.encryption_key,
    ) {
        Ok(s) => s,
        Err(_) => return HttpResponse::InternalServerError().finish(),
    };
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(serde_json::json!({"webhook_url":merchant.webhook_url,"webhook_secret":secret}))
}
/// Finalize setup only if it does not replace a different integration.
pub async fn connect_shopify(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match require_full_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let target = "https://connect.cipherpay.app/api/webhook/cipherpay";
    match sqlx::query("UPDATE merchants SET webhook_url = ? WHERE id = ? AND (webhook_url IS NULL OR webhook_url = '' OR webhook_url = ?)")
        .bind(target).bind(&merchant.id).bind(target).execute(pool.get_ref()).await {
        Ok(result) if result.rows_affected()==1 => HttpResponse::Ok().json(serde_json::json!({"ok":true})),
        Ok(_) => HttpResponse::Conflict().json(serde_json::json!({"error":"A different webhook integration is already configured"})),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

#[cfg(test)]
mod payment_email_tests {
    use super::*;
    use actix_web::{
        http::{Method, StatusCode},
        test, App,
    };
    use serde_json::{json, Value};

    async fn fixture() -> (SqlitePool, Config) {
        let pool = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&pool, "email-owner").await;
        crate::repair_tests::merchant(&pool, "other-owner").await;
        sqlx::query(
            "INSERT INTO sessions (id, merchant_id, expires_at, pos_scoped) VALUES
             ('owner-session', 'email-owner', '2099-01-01T00:00:00Z', 0),
             ('pos-session', 'email-owner', '2099-01-01T00:00:00Z', 1),
             ('other-session', 'other-owner', '2099-01-01T00:00:00Z', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut config = Config::from_env().unwrap();
        config.encryption_key = "11".repeat(32);
        config.network = "testnet".to_string();
        config.smtp_pass = None;
        config.smtp_from = None;
        (pool, config)
    }

    async fn request(
        pool: &SqlitePool,
        config: &Config,
        method: Method,
        session: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(config.clone()))
                .service(
                    web::resource("/api/merchants/me")
                        .route(web::get().to(me))
                        .route(web::patch().to(update_me)),
                ),
        )
        .await;
        let mut req = test::TestRequest::default()
            .uri("/api/merchants/me")
            .method(method);
        if let Some(session) = session {
            req = req.cookie(Cookie::new(SESSION_COOKIE_LEGACY, session.to_string()));
        }
        if let Some(body) = body {
            req = req.set_json(body);
        }
        let response = test::call_service(&app, req.to_request()).await;
        let status = response.status();
        let body = test::read_body_json(response).await;
        (status, body)
    }

    #[actix_web::test]
    async fn payment_email_requires_an_email_before_changing_any_settings() {
        let (pool, config) = fixture().await;
        let (status, body) =
            request(&pool, &config, Method::GET, Some("owner-session"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["payment_email_notifications"], false);
        let (status, body) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "name": "Should not be written",
                "crosschain_enabled": false,
                "payment_email_notifications": true,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["field"], "payment_email_notifications");
        let saved: (String, bool, bool) = sqlx::query_as(
            "SELECT name, crosschain_enabled, payment_email_notifications FROM merchants WHERE id = 'email-owner'",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(saved, (String::new(), true, false));
    }

    #[actix_web::test]
    async fn payment_email_preference_can_enable_disable_and_use_encrypted_saved_email() {
        let (pool, config) = fixture().await;
        let (status, _) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "recovery_email": "owner@example.com",
                "payment_email_notifications": true,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let encrypted: String =
            sqlx::query_scalar("SELECT recovery_email FROM merchants WHERE id = 'email-owner'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(encrypted, "owner@example.com");
        assert_eq!(
            crate::crypto::decrypt_email(&encrypted, &config.encryption_key).unwrap(),
            "owner@example.com"
        );
        let (status, body) =
            request(&pool, &config, Method::GET, Some("owner-session"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["payment_email_notifications"], true);
        assert_eq!(body["recovery_email_preview"], "ow***@example.com");
        for enabled in [false, true] {
            let (status, _) = request(
                &pool,
                &config,
                Method::PATCH,
                Some("owner-session"),
                Some(json!({
                    "payment_email_notifications": enabled,
                })),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let saved: (String, bool) = sqlx::query_as("SELECT recovery_email, payment_email_notifications FROM merchants WHERE id = 'email-owner'")
                .fetch_one(&pool).await.unwrap();
            assert_eq!(saved, (encrypted.clone(), enabled));
        }
        let (status, _) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "name": "Changed name",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) =
            request(&pool, &config, Method::GET, Some("other-session"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["payment_email_notifications"], false);
        let enabled: bool = sqlx::query_scalar(
            "SELECT payment_email_notifications FROM merchants WHERE id = 'email-owner'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(enabled);
    }

    #[actix_web::test]
    async fn payment_email_removal_disables_notifications_and_conflicting_enable_is_rejected() {
        let (pool, config) = fixture().await;
        let (status, _) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "recovery_email": "owner@example.com",
                "payment_email_notifications": true,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "recovery_email": "",
                "payment_email_notifications": true,
                "name": "Should not change",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "recovery_email": "",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let saved: (Option<String>, Option<String>, bool, String) = sqlx::query_as(
            "SELECT recovery_email, recovery_email_hash, payment_email_notifications, name FROM merchants WHERE id = 'email-owner'",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(saved, (None, None, false, String::new()));
        sqlx::query(
            "UPDATE merchants SET recovery_email = 'owner@invalid' WHERE id = 'email-owner'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (status, _) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "payment_email_notifications": true,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn payment_email_settings_reject_unauthenticated_and_pos_sessions() {
        let (pool, config) = fixture().await;
        for session in [None, Some("pos-session")] {
            let (status, _) = request(
                &pool,
                &config,
                Method::PATCH,
                session,
                Some(json!({
                    "recovery_email": "owner@example.com",
                    "payment_email_notifications": true,
                })),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let enabled: bool = sqlx::query_scalar(
            "SELECT payment_email_notifications FROM merchants WHERE id = 'email-owner'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!enabled);
    }

    #[actix_web::test]
    async fn payment_email_storage_errors_roll_back_other_settings_changes() {
        let (pool, config) = fixture().await;
        sqlx::query(
            "UPDATE merchants SET recovery_email = 'owner@example.com' WHERE id = 'email-owner'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_payment_email_preference BEFORE UPDATE OF payment_email_notifications ON merchants
             BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END",
        ).execute(&pool).await.unwrap();
        let (status, _) = request(
            &pool,
            &config,
            Method::PATCH,
            Some("owner-session"),
            Some(json!({
                "name": "Should roll back",
                "payment_email_notifications": true,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let saved: (String, bool) = sqlx::query_as(
            "SELECT name, payment_email_notifications FROM merchants WHERE id = 'email-owner'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(saved, (String::new(), false));
    }
}
