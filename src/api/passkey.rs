use actix_web::{web, HttpRequest, HttpResponse};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{Duration, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::config::Config;

const CHALLENGE_EXPIRY_SECS: i64 = 60;
const REAUTH_WINDOW_SECS: i64 = 300; // 5 minutes

fn build_webauthn(config: &Config) -> Result<webauthn_rs::Webauthn, WebauthnError> {
    let rp_origin =
        url::Url::parse(&config.webauthn_rp_origin).map_err(|_| WebauthnError::Configuration)?;
    let builder = WebauthnBuilder::new(&config.webauthn_rp_id, &rp_origin)?;
    builder.build()
}

// --- Challenge helpers ---

async fn store_challenge(
    pool: &SqlitePool,
    merchant_id: Option<&str>,
    flow_type: &str,
    state_json: &str,
) -> Result<String, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + Duration::seconds(CHALLENGE_EXPIRY_SECS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    sqlx::query(
        "INSERT INTO passkey_challenges (id, merchant_id, flow_type, state_json, expires_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(flow_type)
    .bind(state_json)
    .bind(&expires_at)
    .execute(pool)
    .await?;

    Ok(id)
}

struct ChallengeRecord {
    merchant_id: Option<String>,
    #[allow(dead_code)]
    flow_type: String,
    state_json: String,
}

async fn consume_challenge(
    pool: &SqlitePool,
    challenge_id: &str,
    expected_flow: &str,
) -> Result<Option<ChallengeRecord>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Option<String>, String, String)>(
        "SELECT merchant_id, flow_type, state_json FROM passkey_challenges
         WHERE id = ? AND flow_type = ? AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(challenge_id)
    .bind(expected_flow)
    .fetch_optional(pool)
    .await?;

    if let Some((merchant_id, flow_type, state_json)) = row {
        sqlx::query("DELETE FROM passkey_challenges WHERE id = ?")
            .bind(challenge_id)
            .execute(pool)
            .await?;

        Ok(Some(ChallengeRecord {
            merchant_id,
            flow_type,
            state_json,
        }))
    } else {
        Ok(None)
    }
}

// --- Re-auth helpers ---

async fn check_reauth(pool: &SqlitePool, session_id: &str) -> bool {
    let row = sqlx::query_scalar::<_, Option<String>>(
        "SELECT reauth_at FROM sessions WHERE id = ? AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();

    match row {
        Some(reauth_at) => {
            if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(&reauth_at, "%Y-%m-%dT%H:%M:%SZ")
            {
                let reauth_time = ts.and_utc();
                Utc::now().signed_duration_since(reauth_time).num_seconds() < REAUTH_WINDOW_SECS
            } else {
                false
            }
        }
        None => false,
    }
}

async fn set_reauth(pool: &SqlitePool, session_id: &str) -> Result<(), sqlx::Error> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query("UPDATE sessions SET reauth_at = ? WHERE id = ?")
        .bind(&now)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

// --- Registration ---

/// POST /api/auth/passkey/register/begin
/// Requires active session + recent re-auth
pub async fn register_begin(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let session_id = match super::auth::extract_session_id(&req) {
        Some(id) => id,
        None => return super::auth::not_authenticated_response(),
    };

    if !check_reauth(pool.get_ref(), &session_id).await {
        tracing::info!(merchant_id = %merchant.id, "Passkey register/begin rejected: reauth required");
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Recent re-authentication required",
            "code": "reauth_required"
        }));
    }

    tracing::info!(merchant_id = %merchant.id, "Passkey register/begin: reauth valid, starting registration");

    let webauthn = match build_webauthn(&config) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = ?e, "WebAuthn configuration error");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "WebAuthn not configured"}));
        }
    };

    let user_id = Uuid::parse_str(&merchant.id).unwrap_or_else(|_| Uuid::new_v4());
    let friendly_name = if merchant.name.is_empty() {
        "CipherPay Merchant".to_string()
    } else {
        merchant.name.clone()
    };

    let existing = load_merchant_passkeys(pool.get_ref(), &merchant.id).await;

    let exclude = if existing.is_empty() {
        None
    } else {
        Some(existing.iter().map(|pk| pk.cred_id().clone()).collect::<Vec<_>>())
    };

    let (ccr, reg_state) = match webauthn.start_passkey_registration(
        user_id,
        &friendly_name,
        &friendly_name,
        exclude,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to start passkey registration");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Registration failed"}));
        }
    };

    let state_json = match serde_json::to_string(&reg_state) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize registration state");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let challenge_id =
        match store_challenge(pool.get_ref(), Some(&merchant.id), "register", &state_json).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, "Failed to store challenge");
                return HttpResponse::InternalServerError()
                    .json(serde_json::json!({"error": "Internal error"}));
            }
        };

    HttpResponse::Ok().json(serde_json::json!({
        "challenge_id": challenge_id,
        "options": ccr,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RegisterCompleteRequest {
    pub challenge_id: String,
    pub credential: RegisterPublicKeyCredential,
    pub label: Option<String>,
}

/// POST /api/auth/passkey/register/complete
pub async fn register_complete(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<RegisterCompleteRequest>,
) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let challenge = match consume_challenge(pool.get_ref(), &body.challenge_id, "register").await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "Invalid or expired challenge"}));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to consume challenge");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    if challenge.merchant_id.as_deref() != Some(&merchant.id) {
        return HttpResponse::Forbidden()
            .json(serde_json::json!({"error": "Challenge does not match session"}));
    }

    let webauthn = match build_webauthn(&config) {
        Ok(w) => w,
        Err(_) => {
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "WebAuthn not configured"}));
        }
    };

    let reg_state: PasskeyRegistration = match serde_json::from_str(&challenge.state_json) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to deserialize registration state");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let passkey = match webauthn.finish_passkey_registration(&body.credential, &reg_state) {
        Ok(pk) => pk,
        Err(e) => {
            tracing::error!(error = ?e, "Passkey registration verification failed");
            return HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "Registration verification failed"}));
        }
    };

    let cred_id = URL_SAFE_NO_PAD.encode(passkey.cred_id().as_ref());
    let credential_json = match serde_json::to_string(&passkey) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize passkey");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let label = body
        .label
        .as_deref()
        .unwrap_or("Passkey")
        .chars()
        .take(64)
        .collect::<String>();

    if let Err(e) = sqlx::query(
        "INSERT INTO passkey_credentials (id, merchant_id, credential_json, label) VALUES (?, ?, ?, ?)",
    )
    .bind(&cred_id)
    .bind(&merchant.id)
    .bind(&credential_json)
    .bind(&label)
    .execute(pool.get_ref())
    .await
    {
        tracing::error!(error = %e, "Failed to store passkey credential");
        return HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "Failed to store credential"}));
    }

    tracing::info!(merchant_id = %merchant.id, label = %label, "Passkey registered");

    HttpResponse::Ok().json(serde_json::json!({
        "status": "registered",
        "credential_id": cred_id,
        "label": label,
    }))
}

// --- Authentication ---

/// POST /api/auth/passkey/login/begin
/// Public endpoint (no auth required)
pub async fn login_begin(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
) -> HttpResponse {
    let webauthn = match build_webauthn(&config) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = ?e, "WebAuthn configuration error");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "WebAuthn not configured"}));
        }
    };

    let all_creds = load_all_passkeys(pool.get_ref()).await;
    tracing::info!(credential_count = all_creds.len(), "Passkey login/begin: loaded credentials");
    if all_creds.is_empty() {
        tracing::warn!("Passkey login/begin: no passkeys registered");
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "No passkeys registered on this instance"
        }));
    }

    let (rcr, auth_state) = match webauthn.start_passkey_authentication(&all_creds) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to start passkey authentication");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Authentication failed"}));
        }
    };

    let state_json = match serde_json::to_string(&auth_state) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize auth state");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let challenge_id =
        match store_challenge(pool.get_ref(), None, "login", &state_json).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, "Failed to store challenge");
                return HttpResponse::InternalServerError()
                    .json(serde_json::json!({"error": "Internal error"}));
            }
        };

    HttpResponse::Ok().json(serde_json::json!({
        "challenge_id": challenge_id,
        "options": rcr,
    }))
}

#[derive(Debug, Deserialize)]
pub struct LoginCompleteRequest {
    pub challenge_id: String,
    pub credential: PublicKeyCredential,
}

/// POST /api/auth/passkey/login/complete
pub async fn login_complete(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<LoginCompleteRequest>,
) -> HttpResponse {
    let challenge = match consume_challenge(pool.get_ref(), &body.challenge_id, "login").await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "Invalid or expired challenge"}));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to consume challenge");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let webauthn = match build_webauthn(&config) {
        Ok(w) => w,
        Err(_) => {
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "WebAuthn not configured"}));
        }
    };

    let auth_state: PasskeyAuthentication = match serde_json::from_str(&challenge.state_json) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to deserialize auth state");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let auth_result = match webauthn.finish_passkey_authentication(&body.credential, &auth_state) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = ?e, "Passkey authentication failed");
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({"error": "Authentication failed"}));
        }
    };

    // Find which merchant owns this credential
    let cred_id_b64 = URL_SAFE_NO_PAD.encode(auth_result.cred_id().as_ref());
    let merchant_id = match sqlx::query_scalar::<_, String>(
        "SELECT merchant_id FROM passkey_credentials WHERE id = ?",
    )
    .bind(&cred_id_b64)
    .fetch_optional(pool.get_ref())
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({"error": "Credential not found"}));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to look up credential");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    // Update counter and last_used_at
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Re-load and update the stored passkey with new counter
    if let Ok(Some(cred_json)) = sqlx::query_scalar::<_, String>(
        "SELECT credential_json FROM passkey_credentials WHERE id = ?",
    )
    .bind(&cred_id_b64)
    .fetch_optional(pool.get_ref())
    .await
    {
        if let Ok(mut passkey) = serde_json::from_str::<Passkey>(&cred_json) {
            passkey.update_credential(&auth_result);
            if let Ok(updated_json) = serde_json::to_string(&passkey) {
                let _ = sqlx::query(
                    "UPDATE passkey_credentials SET credential_json = ?, last_used_at = ? WHERE id = ?",
                )
                .bind(&updated_json)
                .bind(&now)
                .bind(&cred_id_b64)
                .execute(pool.get_ref())
                .await;
            }
        }
    }

    // Create session (same as token login)
    let session_id = Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + chrono::Duration::hours(24))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    if let Err(e) =
        sqlx::query("INSERT INTO sessions (id, merchant_id, expires_at) VALUES (?, ?, ?)")
            .bind(&session_id)
            .bind(&merchant_id)
            .bind(&expires_at)
            .execute(pool.get_ref())
            .await
    {
        tracing::error!(error = %e, "Failed to create session");
        return HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "Failed to create session"}));
    }

    let cookie = super::auth::build_session_cookie(&session_id, &config, false);

    tracing::info!(merchant_id = %merchant_id, "Passkey login successful");

    HttpResponse::Ok().cookie(cookie).json(serde_json::json!({
        "merchant_id": merchant_id,
        "auth_method": "passkey",
    }))
}

// --- Re-auth ---

#[derive(Debug, Deserialize)]
pub struct ReauthRequest {
    pub token: Option<String>,
    pub passkey_challenge_id: Option<String>,
    pub passkey_credential: Option<PublicKeyCredential>,
}

/// POST /api/auth/passkey/reauth
/// Re-authenticate via dashboard token OR passkey assertion
pub async fn reauth(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<ReauthRequest>,
) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let session_id = match super::auth::extract_session_id(&req) {
        Some(id) => id,
        None => return super::auth::not_authenticated_response(),
    };

    // Method 1: Re-auth via dashboard token
    if let Some(ref token) = body.token {
        match crate::merchants::authenticate_dashboard(
            pool.get_ref(),
            token,
            &config.encryption_key,
        )
        .await
        {
            Ok(Some(m)) if m.id == merchant.id => {
                if let Err(e) = set_reauth(pool.get_ref(), &session_id).await {
                    tracing::error!(error = %e, "Failed to set reauth timestamp");
                    return HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "Internal error"}));
                }
                tracing::info!(merchant_id = %merchant.id, "Reauth via token successful");
                return HttpResponse::Ok()
                    .json(serde_json::json!({"status": "reauth_complete"}));
            }
            _ => {
                tracing::warn!(merchant_id = %merchant.id, "Reauth via token failed: invalid token");
                return HttpResponse::Unauthorized()
                    .json(serde_json::json!({"error": "Invalid dashboard token"}));
            }
        }
    }

    // Method 2: Re-auth via passkey assertion
    if let (Some(ref challenge_id), Some(ref credential)) =
        (&body.passkey_challenge_id, &body.passkey_credential)
    {
        let challenge = match consume_challenge(pool.get_ref(), challenge_id, "login").await {
            Ok(Some(c)) => c,
            _ => {
                return HttpResponse::BadRequest()
                    .json(serde_json::json!({"error": "Invalid or expired challenge"}));
            }
        };

        let webauthn = match build_webauthn(&config) {
            Ok(w) => w,
            Err(_) => {
                return HttpResponse::InternalServerError()
                    .json(serde_json::json!({"error": "WebAuthn not configured"}));
            }
        };

        let auth_state: PasskeyAuthentication =
            match serde_json::from_str(&challenge.state_json) {
                Ok(s) => s,
                Err(_) => {
                    return HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "Internal error"}));
                }
            };

        match webauthn.finish_passkey_authentication(credential, &auth_state) {
            Ok(result) => {
                // Verify the credential belongs to this merchant
                let cred_id_b64 = URL_SAFE_NO_PAD.encode(result.cred_id().as_ref());
                let owner = sqlx::query_scalar::<_, String>(
                    "SELECT merchant_id FROM passkey_credentials WHERE id = ?",
                )
                .bind(&cred_id_b64)
                .fetch_optional(pool.get_ref())
                .await
                .ok()
                .flatten();

                if owner.as_deref() != Some(&merchant.id) {
                    return HttpResponse::Forbidden()
                        .json(serde_json::json!({"error": "Credential does not match session"}));
                }

                if let Err(e) = set_reauth(pool.get_ref(), &session_id).await {
                    tracing::error!(error = %e, "Failed to set reauth timestamp");
                    return HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "Internal error"}));
                }

                return HttpResponse::Ok()
                    .json(serde_json::json!({"status": "reauth_complete"}));
            }
            Err(e) => {
                tracing::error!(error = ?e, "Passkey reauth failed");
                return HttpResponse::Unauthorized()
                    .json(serde_json::json!({"error": "Passkey verification failed"}));
            }
        }
    }

    HttpResponse::BadRequest().json(serde_json::json!({
        "error": "Provide either 'token' or 'passkey_challenge_id' + 'passkey_credential'"
    }))
}

// --- Management ---

/// GET /api/auth/passkeys
pub async fn list_passkeys(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let rows = sqlx::query_as::<_, (String, String, Option<String>, String)>(
        "SELECT id, label, last_used_at, created_at FROM passkey_credentials
         WHERE merchant_id = ? ORDER BY created_at DESC",
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await
    .unwrap_or_default();

    let passkeys: Vec<serde_json::Value> = rows
        .iter()
        .map(|(id, label, last_used, created)| {
            serde_json::json!({
                "id": id,
                "label": label,
                "last_used_at": last_used,
                "created_at": created,
            })
        })
        .collect();

    HttpResponse::Ok().json(serde_json::json!({ "passkeys": passkeys }))
}

/// DELETE /api/auth/passkeys/{id}
/// Requires recent re-auth
pub async fn delete_passkey(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let session_id = match super::auth::extract_session_id(&req) {
        Some(id) => id,
        None => return super::auth::not_authenticated_response(),
    };

    if !check_reauth(pool.get_ref(), &session_id).await {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Recent re-authentication required",
            "code": "reauth_required"
        }));
    }

    let cred_id = path.into_inner();

    let result = sqlx::query(
        "DELETE FROM passkey_credentials WHERE id = ? AND merchant_id = ?",
    )
    .bind(&cred_id)
    .bind(&merchant.id)
    .execute(pool.get_ref())
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::info!(merchant_id = %merchant.id, cred_id = %cred_id, "Passkey removed");
            HttpResponse::Ok().json(serde_json::json!({"status": "deleted"}))
        }
        Ok(_) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": "Passkey not found"}))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete passkey");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}))
        }
    }
}

// --- Helpers ---

async fn load_merchant_passkeys(pool: &SqlitePool, merchant_id: &str) -> Vec<Passkey> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT credential_json FROM passkey_credentials WHERE merchant_id = ?",
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows.iter()
        .filter_map(|(json,)| serde_json::from_str::<Passkey>(json).ok())
        .collect()
}

async fn load_all_passkeys(pool: &SqlitePool) -> Vec<Passkey> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT credential_json FROM passkey_credentials")
            .fetch_all(pool)
            .await
            .unwrap_or_default();

    rows.iter()
        .filter_map(|(json,)| serde_json::from_str::<Passkey>(json).ok())
        .collect()
}

/// Check if a merchant has any registered passkeys
pub async fn has_passkeys(pool: &SqlitePool, merchant_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM passkey_credentials WHERE merchant_id = ?",
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
        > 0
}

/// Purge expired challenges (called from hourly data purge)
pub async fn purge_expired_challenges(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM passkey_challenges WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
