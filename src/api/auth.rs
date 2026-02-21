use actix_web::{web, HttpRequest, HttpResponse};
use actix_web::cookie::{Cookie, SameSite};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;
use crate::merchants;

const SESSION_COOKIE: &str = "cpay_session";
const SESSION_DAYS: i64 = 30;

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
    let merchant = match merchants::authenticate_dashboard(pool.get_ref(), &body.token).await {
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
    let expires_at = (Utc::now() + Duration::days(SESSION_DAYS))
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

    HttpResponse::Ok().json(serde_json::json!({
        "id": merchant.id,
        "payment_address": merchant.payment_address,
        "webhook_url": merchant.webhook_url,
        "has_recovery_email": merchant.recovery_email.is_some(),
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
         price_eur, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         shipping_alias, shipping_address,
         shipping_region, status, detected_txid, detected_at,
         confirmed_at, shipped_at, expires_at, purge_after, created_at
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
    merchants::get_by_session(pool, &session_id).await.ok()?
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
        builder = builder.max_age(actix_web::cookie::time::Duration::days(SESSION_DAYS));
    }

    builder.finish()
}

#[derive(Debug, Deserialize)]
pub struct UpdateMerchantRequest {
    pub payment_address: Option<String>,
    pub webhook_url: Option<String>,
}

/// PATCH /api/merchants/me -- update payment address and/or webhook URL
pub async fn update_me(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
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

    if let Some(ref addr) = body.payment_address {
        if addr.is_empty() {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Payment address cannot be empty"
            }));
        }
        sqlx::query("UPDATE merchants SET payment_address = ? WHERE id = ?")
            .bind(addr)
            .bind(&merchant.id)
            .execute(pool.get_ref())
            .await
            .ok();
        tracing::info!(merchant_id = %merchant.id, "Payment address updated");
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

    HttpResponse::Ok().json(serde_json::json!({ "status": "updated" }))
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
