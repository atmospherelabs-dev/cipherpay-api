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

    let has_passkeys = super::passkey::has_passkeys(pool.get_ref(), &merchant.id).await;

    let last_token_login: Option<String> = sqlx::query_scalar(
        "SELECT last_token_login_at FROM merchants WHERE id = ?",
    )
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
        "created_at": merchant.created_at,
        "stats": stats,
        "has_luma_key": has_luma_key,
        "has_passkeys": has_passkeys,
        "last_token_login_at": last_token_login,
    }))
}

/// GET /api/merchants/me/invoices -- list invoices for the authenticated merchant
pub async fn my_invoices(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
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
         payment_link_id, is_donation, campaign_counted
         FROM invoices WHERE merchant_id = ?
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(invoices) => {
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

            let enriched: Vec<serde_json::Value> = invoices
                .iter()
                .map(|inv| {
                    let mut val = serde_json::to_value(inv).unwrap_or(serde_json::json!({}));
                    if let serde_json::Value::Object(ref mut map) = val {
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
    let merchant = match resolve_session(&req, &pool).await {
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
    let session_id = extract_session_id(req)?;
    let config = req.app_data::<web::Data<crate::config::Config>>()?;
    merchants::get_by_session(pool, &session_id, &config.encryption_key)
        .await
        .ok()?
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
            if key.starts_with("cpay_sk_") || key.starts_with("cpay_rk_") || key.starts_with("cpay_")
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

pub async fn require_merchant_or_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Result<merchants::Merchant, HttpResponse> {
    resolve_merchant_or_session(req, pool)
        .await
        .ok_or_else(not_authenticated_response)
}

/// Reject restricted API keys. Use for endpoints that mutate account-level
/// configuration: PATCH /me, key/secret rotation, billing actions, account
/// deletion, key management. Restricted keys get 403 with a clear error code.
pub async fn require_full_or_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Result<merchants::Merchant, HttpResponse> {
    match resolve_with_kind(req, pool).await {
        Some((m, kind)) => {
            if matches!(kind, merchants::KeyKind::Restricted) {
                Err(restricted_key_forbidden_response())
            } else {
                Ok(m)
            }
        }
        None => Err(not_authenticated_response()),
    }
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

#[derive(Debug, Deserialize)]
pub struct UpdateMerchantRequest {
    pub name: Option<String>,
    pub webhook_url: Option<String>,
    pub recovery_email: Option<String>,
    pub luma_api_key: Option<String>,
}

/// PATCH /api/merchants/me -- update name, webhook URL, and/or recovery email.
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
    let merchant = match resolve_session(&req, &pool).await {
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

    if let Some(ref name) = body.name {
        sqlx::query("UPDATE merchants SET name = ? WHERE id = ?")
            .bind(name)
            .bind(&merchant.id)
            .execute(pool.get_ref())
            .await
            .ok();
        tracing::info!(merchant_id = %merchant.id, "Merchant name updated");
    }

    if let Some(ref url) = body.webhook_url {
        sqlx::query("UPDATE merchants SET webhook_url = ? WHERE id = ?")
            .bind(if url.is_empty() {
                None
            } else {
                Some(url.as_str())
            })
            .bind(&merchant.id)
            .execute(pool.get_ref())
            .await
            .ok();
        tracing::info!(merchant_id = %merchant.id, "Webhook URL updated");
    }

    if let Some(ref email) = body.recovery_email {
        if email.is_empty() {
            sqlx::query("UPDATE merchants SET recovery_email = NULL, recovery_email_hash = NULL WHERE id = ?")
                .bind(&merchant.id)
                .execute(pool.get_ref())
                .await
                .ok();
        } else {
            let encrypted = if config.encryption_key.is_empty() {
                email.clone()
            } else {
                match crate::crypto::encrypt(email, &config.encryption_key) {
                    Ok(enc) => enc,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to encrypt recovery email");
                        return HttpResponse::InternalServerError()
                            .json(serde_json::json!({"error": "Internal error"}));
                    }
                }
            };
            let hash = crate::crypto::blind_index(email, &config.encryption_key);
            sqlx::query(
                "UPDATE merchants SET recovery_email = ?, recovery_email_hash = ? WHERE id = ?",
            )
            .bind(&encrypted)
            .bind(&hash)
            .bind(&merchant.id)
            .execute(pool.get_ref())
            .await
            .ok();
        }
        tracing::info!(merchant_id = %merchant.id, "Recovery email updated");
    }

    if let Some(ref key) = body.luma_api_key {
        if key.is_empty() {
            sqlx::query("UPDATE merchants SET luma_api_key = NULL WHERE id = ?")
                .bind(&merchant.id)
                .execute(pool.get_ref())
                .await
                .ok();
        } else {
            let encrypted = if config.encryption_key.is_empty() {
                key.clone()
            } else {
                match crate::crypto::encrypt(key, &config.encryption_key) {
                    Ok(enc) => enc,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to encrypt Luma API key");
                        return HttpResponse::InternalServerError()
                            .json(serde_json::json!({"error": "Internal error"}));
                    }
                }
            };
            sqlx::query("UPDATE merchants SET luma_api_key = ? WHERE id = ?")
                .bind(&encrypted)
                .bind(&merchant.id)
                .execute(pool.get_ref())
                .await
                .ok();
        }
        tracing::info!(merchant_id = %merchant.id, "Luma API key updated");
    }

    HttpResponse::Ok().json(serde_json::json!({ "status": "updated" }))
}

/// POST /api/merchants/me/regenerate-api-key
pub async fn regenerate_api_key(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
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
    let merchant = match resolve_session(&req, &pool).await {
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
    let merchant = match resolve_session(&req, &pool).await {
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
