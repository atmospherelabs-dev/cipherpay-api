//! HTTP handlers for `/api/merchants/me/keys` — restricted API key management.
//!
//! Every handler here is gated by [`crate::api::auth::require_full_or_session`]:
//! a restricted key cannot list, create, rotate, or revoke other keys.

use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::api_keys::{self, KeyType, RevokeOutcome};
use crate::validation;

#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    /// `"full"` or `"restricted"`. See `KeyType`.
    #[serde(rename = "type")]
    pub key_type: String,
    pub label: String,
}

/// POST /api/merchants/me/keys — mint a new key.
///
/// Returns the raw key exactly once (`{ "key": "cpay_rk_..." }`). The dashboard
/// must surface it immediately and warn the user it cannot be retrieved later.
pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreateKeyRequest>,
) -> HttpResponse {
    let merchant = match super::auth::require_full_or_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    let key_type = match body.key_type.as_str() {
        "full" => KeyType::Full,
        "restricted" => KeyType::Restricted,
        _ => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "type must be 'full' or 'restricted'"
            }));
        }
    };

    let label = body.label.trim();
    if let Err(e) = validation::validate_length("label", label, 100) {
        return HttpResponse::BadRequest().json(e.to_json());
    }
    if label.is_empty() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "label is required"
        }));
    }

    match api_keys::create_key(pool.get_ref(), &merchant.id, key_type, label).await {
        Ok(created) => HttpResponse::Created().json(created),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create API key");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create API key"
            }))
        }
    }
}

/// GET /api/merchants/me/keys — list non-revoked keys (label, prefix, type, timestamps).
///
/// Never returns the raw key or hash. Restricted keys are denied.
pub async fn list(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match super::auth::require_full_or_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    match api_keys::list_keys(pool.get_ref(), &merchant.id).await {
        Ok(keys) => HttpResponse::Ok().json(serde_json::json!({ "keys": keys })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list API keys");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to list API keys"
            }))
        }
    }
}

/// DELETE /api/merchants/me/keys/:id — revoke a key.
///
/// Returns 409 if this is the merchant's only remaining full-access credential
/// (the dashboard token would still work, but the API would be locked out).
pub async fn delete(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match super::auth::require_full_or_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    let key_id = path.into_inner();
    match api_keys::revoke_key(pool.get_ref(), &merchant.id, &key_id).await {
        Ok(RevokeOutcome::Revoked) => {
            HttpResponse::Ok().json(serde_json::json!({ "status": "revoked" }))
        }
        Ok(RevokeOutcome::NotFound) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Key not found"
        })),
        Ok(RevokeOutcome::LastFullKey) => HttpResponse::Conflict().json(serde_json::json!({
            "error": "Cannot revoke the only remaining full-access key. Create another full key first.",
            "code": "last_full_key",
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to revoke API key");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to revoke API key"
            }))
        }
    }
}
