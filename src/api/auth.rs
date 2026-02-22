use actix_web::{web, HttpRequest, HttpResponse};
use actix_web::cookie::{Cookie, SameSite};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;
use crate::merchants;
use crate::validation;

const SESSION_COOKIE: &str = "cpay_session";
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
    let merchant = match merchants::authenticate_dashboard(pool.get_ref(), &body.token, &config.encryption_key).await {
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

    if let Err(e) = sqlx::query(
        "INSERT INTO sessions (id, merchant_id, expires_at) VALUES (?, ?, ?)"
    )
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

    let cookie = build_session_cookie(&session_id, &config, false);

    HttpResponse::Ok()
        .cookie(cookie)
        .json(serde_json::json!({
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
pub async fn me(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
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
            format!("{}{}{}",
                &local[..visible],
                "*".repeat(local.len().saturating_sub(visible)),
                domain
            )
        } else {
            "***".to_string()
        }
    });

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
    }))
}

/// GET /api/merchants/me/invoices -- list invoices for the authenticated merchant
pub async fn my_invoices(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let rows = sqlx::query_as::<_, crate::invoices::Invoice>(
        "SELECT id, merchant_id, memo_code, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         NULL AS merchant_name,
         refund_address, status, detected_txid, detected_at,
         confirmed_at, refunded_at, expires_at, purge_after, created_at,
         orchard_receiver_hex, diversifier_index
         FROM invoices WHERE merchant_id = ?
         ORDER BY created_at DESC LIMIT 100"
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(invoices) => HttpResponse::Ok().json(invoices),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list merchant invoices");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

/// Extract the session ID from the cpay_session cookie
pub fn extract_session_id(req: &HttpRequest) -> Option<String> {
    req.cookie(SESSION_COOKIE)
        .map(|c| c.value().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve a merchant from the session cookie
pub async fn resolve_session(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Option<merchants::Merchant> {
    let session_id = extract_session_id(req)?;
    let config = req.app_data::<web::Data<crate::config::Config>>()?;
    merchants::get_by_session(pool, &session_id, &config.encryption_key).await.ok()?
}

fn build_session_cookie<'a>(value: &str, config: &Config, clear: bool) -> Cookie<'a> {
    let mut builder = Cookie::build(SESSION_COOKIE, value.to_string())
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax);

    if !config.is_testnet() {
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
            .bind(if url.is_empty() { None } else { Some(url.as_str()) })
            .bind(&merchant.id)
            .execute(pool.get_ref())
            .await
            .ok();
        tracing::info!(merchant_id = %merchant.id, "Webhook URL updated");
    }

    if let Some(ref email) = body.recovery_email {
        let val = if email.is_empty() { None } else { Some(email.as_str()) };
        sqlx::query("UPDATE merchants SET recovery_email = ? WHERE id = ?")
            .bind(val)
            .bind(&merchant.id)
            .execute(pool.get_ref())
            .await
            .ok();
        tracing::info!(merchant_id = %merchant.id, "Recovery email updated");
    }

    HttpResponse::Ok().json(serde_json::json!({ "status": "updated" }))
}

/// POST /api/merchants/me/regenerate-api-key
pub async fn regenerate_api_key(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
        Some(m) => m,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({ "error": "Not authenticated" })),
    };

    match merchants::regenerate_api_key(pool.get_ref(), &merchant.id).await {
        Ok(new_key) => HttpResponse::Ok().json(serde_json::json!({ "api_key": new_key })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to regenerate API key");
            HttpResponse::InternalServerError().json(serde_json::json!({ "error": "Failed to regenerate" }))
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
        None => return HttpResponse::Unauthorized().json(serde_json::json!({ "error": "Not authenticated" })),
    };

    match merchants::regenerate_dashboard_token(pool.get_ref(), &merchant.id).await {
        Ok(new_token) => HttpResponse::Ok().json(serde_json::json!({ "dashboard_token": new_token })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to regenerate dashboard token");
            HttpResponse::InternalServerError().json(serde_json::json!({ "error": "Failed to regenerate" }))
        }
    }
}

/// POST /api/merchants/me/regenerate-webhook-secret
pub async fn regenerate_webhook_secret(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
        Some(m) => m,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({ "error": "Not authenticated" })),
    };

    match merchants::regenerate_webhook_secret(pool.get_ref(), &merchant.id).await {
        Ok(new_secret) => HttpResponse::Ok().json(serde_json::json!({ "webhook_secret": new_secret })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to regenerate webhook secret");
            HttpResponse::InternalServerError().json(serde_json::json!({ "error": "Failed to regenerate" }))
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
        let merchant = match merchants::find_by_email(pool.get_ref(), &body.email, &config.encryption_key).await {
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
    }.await;

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
        Ok(Some(new_dashboard_token)) => {
            HttpResponse::Ok().json(serde_json::json!({
                "dashboard_token": new_dashboard_token,
                "message": "Account recovered. Save your new dashboard token."
            }))
        }
        Ok(None) => {
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
            COALESCE(SUM(CASE WHEN status = 'confirmed' THEN price_zec ELSE 0 END), 0.0) as total_zec
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
