use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::merchants;

const POS_SESSION_HOURS: i64 = 4;

#[derive(Debug, Deserialize)]
pub struct SetPinRequest {
    pub pin: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyPinRequest {
    pub pin: String,
}

/// PUT /api/merchants/me/pos-pin — set or change POS PIN (requires full dashboard auth)
pub async fn set_pin(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<SetPinRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => return super::auth::not_authenticated_response(),
    };

    if body.pin.is_empty() {
        // Remove PIN
        if let Err(e) = sqlx::query("UPDATE merchants SET pos_pin_hash = NULL WHERE id = ?")
            .bind(&merchant.id)
            .execute(pool.get_ref())
            .await
        {
            tracing::error!(error = %e, "Failed to remove POS PIN");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to update PIN"
            }));
        }
        return HttpResponse::Ok().json(serde_json::json!({ "status": "removed" }));
    }

    if body.pin.len() != 4 || !body.pin.chars().all(|c| c.is_ascii_digit()) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "PIN must be exactly 4 digits"
        }));
    }

    let hash = merchants::hash_key(&body.pin);

    if let Err(e) = sqlx::query("UPDATE merchants SET pos_pin_hash = ? WHERE id = ?")
        .bind(&hash)
        .bind(&merchant.id)
        .execute(pool.get_ref())
        .await
    {
        tracing::error!(error = %e, "Failed to set POS PIN");
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Failed to update PIN"
        }));
    }

    HttpResponse::Ok().json(serde_json::json!({ "status": "set" }))
}

/// GET /api/merchants/me/pos-pin — check if POS PIN is set (requires dashboard auth)
pub async fn has_pin(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => return super::auth::not_authenticated_response(),
    };

    let has_pin: bool = sqlx::query_scalar::<_, Option<String>>(
        "SELECT pos_pin_hash FROM merchants WHERE id = ?",
    )
    .bind(&merchant.id)
    .fetch_optional(pool.get_ref())
    .await
    .ok()
    .flatten()
    .flatten()
    .map(|h| !h.is_empty())
    .unwrap_or(false);

    HttpResponse::Ok().json(serde_json::json!({ "has_pin": has_pin }))
}

/// POST /api/auth/pos-session — verify POS PIN and create scoped session
pub async fn create_pos_session(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<VerifyPinRequest>,
) -> HttpResponse {
    if body.pin.len() != 4 || !body.pin.chars().all(|c| c.is_ascii_digit()) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "PIN must be 4 digits"
        }));
    }

    // If caller already has a valid full session, resolve merchant from that
    // to identify which merchant this POS belongs to.
    let merchant = if let Some(m) = super::auth::resolve_session(&req, &pool).await {
        m
    } else {
        // No session cookie — the POS is accessed standalone.
        // Try to find a merchant by PIN hash across all merchants.
        // For v1 (single POS per device), this works. Multi-tenant would
        // need a merchant identifier in the request.
        let pin_hash = merchants::hash_key(&body.pin);
        match sqlx::query_scalar::<_, String>(
            "SELECT id FROM merchants WHERE pos_pin_hash = ?",
        )
        .bind(&pin_hash)
        .fetch_optional(pool.get_ref())
        .await
        {
            Ok(Some(mid)) => {
                match merchants::get_merchant_by_id(pool.get_ref(), &mid, &config.encryption_key)
                    .await
                {
                    Ok(Some(m)) => m,
                    _ => {
                        return HttpResponse::Unauthorized().json(serde_json::json!({
                            "error": "Invalid PIN"
                        }));
                    }
                }
            }
            Ok(None) => {
                // No merchant found with this PIN — check if any merchant
                // has a PIN set at all. If not, signal that POS PIN is not configured.
                let any_pin_set: bool = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM merchants WHERE pos_pin_hash IS NOT NULL",
                )
                .fetch_one(pool.get_ref())
                .await
                .unwrap_or(0)
                    > 0;

                if !any_pin_set {
                    return HttpResponse::BadRequest().json(serde_json::json!({
                        "error": "pos_pin_not_set"
                    }));
                }

                return HttpResponse::Unauthorized().json(serde_json::json!({
                    "error": "Invalid PIN"
                }));
            }
            Err(e) => {
                tracing::error!(error = %e, "POS PIN lookup failed");
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }));
            }
        }
    };

    // If merchant has a PIN set, verify it. If no PIN is set,
    // fall through only if they have a valid dashboard session.
    let stored_hash: Option<String> = sqlx::query_scalar(
        "SELECT pos_pin_hash FROM merchants WHERE id = ?",
    )
    .bind(&merchant.id)
    .fetch_optional(pool.get_ref())
    .await
    .ok()
    .flatten();

    match stored_hash {
        Some(ref h) if !h.is_empty() => {
            let pin_hash = merchants::hash_key(&body.pin);
            if pin_hash != *h {
                return HttpResponse::Unauthorized().json(serde_json::json!({
                    "error": "Invalid PIN"
                }));
            }
        }
        _ => {
            // No PIN set: only allow if caller has a full dashboard session
            if super::auth::resolve_session(&req, &pool).await.is_none() {
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "pos_pin_not_set"
                }));
            }
        }
    }

    // Create a POS-scoped session
    let session_id = uuid::Uuid::new_v4().to_string();
    let expires_at = (chrono::Utc::now() + chrono::Duration::hours(POS_SESSION_HOURS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    if let Err(e) = sqlx::query(
        "INSERT INTO sessions (id, merchant_id, expires_at, pos_scoped) VALUES (?, ?, ?, 1)",
    )
    .bind(&session_id)
    .bind(&merchant.id)
    .bind(&expires_at)
    .execute(pool.get_ref())
    .await
    {
        tracing::error!(error = %e, "Failed to create POS session");
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Failed to create session"
        }));
    }

    let cookie = super::auth::build_session_cookie(&session_id, &config, false);

    HttpResponse::Ok().cookie(cookie).json(serde_json::json!({
        "merchant_id": merchant.id,
        "merchant_name": merchant.name,
        "pos_scoped": true,
    }))
}
