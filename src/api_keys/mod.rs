//! Restricted API keys.
//!
//! Per-merchant scoped credentials stored in the `merchant_api_keys` table.
//! See `[wiki/entities/cipherpay.md]` for the design rationale and the
//! `cpay_sk_` vs `cpay_rk_` convention.
//!
//! Authentication still flows through [`crate::merchants::authenticate_with_kind`],
//! which reads from this table first and falls back to the legacy
//! `merchants.api_key_hash` column. Scope enforcement is one binary check at
//! the route handler — see [`crate::api::auth::require_full_or_session`].

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::merchants::hash_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyType {
    /// Same access as the legacy `merchants.api_key_hash` — full account control.
    Full,
    /// Limited to invoice/x402/session operations. Denied at the deny-list
    /// endpoints documented in the API reference.
    Restricted,
}

impl KeyType {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyType::Full => "full",
            KeyType::Restricted => "restricted",
        }
    }
}

/// Public-safe representation of a stored key. Never includes the raw key or
/// hash. The `key_prefix` is the first 16 characters of the raw key (e.g.
/// `cpay_sk_a1b2c3d4`) — enough for humans to disambiguate, useless on its own
/// for authentication.
#[derive(Debug, Serialize)]
pub struct ApiKeySummary {
    pub id: String,
    pub label: String,
    pub key_prefix: String,
    pub key_type: KeyType,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// Returned exactly once when a key is minted. The raw `key` is not stored
/// server-side and cannot be retrieved later.
#[derive(Debug, Serialize)]
pub struct CreatedApiKey {
    pub id: String,
    pub key: String,
    pub key_prefix: String,
    pub key_type: KeyType,
    pub label: String,
    pub created_at: String,
}

fn generate_raw_key(key_type: KeyType) -> String {
    let bytes: [u8; 32] = rand::random();
    let prefix = match key_type {
        KeyType::Full => "cpay_sk_",
        KeyType::Restricted => "cpay_rk_",
    };
    format!("{}{}", prefix, hex::encode(bytes))
}

/// First 16 chars: prefix (8) + 8 hex chars from the random body. Stored in the
/// DB so the dashboard can show "rk_a1b2c3d4..." in the keys table.
fn prefix_of(raw: &str) -> String {
    raw.chars().take(16).collect()
}

/// Mint a new key for a merchant. The returned `CreatedApiKey.key` is the only
/// time the raw key is ever exposed — the caller must surface it to the user
/// immediately and warn that it cannot be retrieved.
///
/// `label` is a human-readable name (e.g. "Claude Agent"). Required so the user
/// can identify which agent or service holds which key.
pub async fn create_key(
    pool: &SqlitePool,
    merchant_id: &str,
    key_type: KeyType,
    label: &str,
) -> anyhow::Result<CreatedApiKey> {
    let raw = generate_raw_key(key_type);
    let key_hash = hash_key(&raw);
    let key_prefix = prefix_of(&raw);
    let id = format!("mak_{}", Uuid::new_v4().simple());

    sqlx::query(
        "INSERT INTO merchant_api_keys (id, merchant_id, key_hash, key_prefix, key_type, label)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(key_type.as_str())
    .bind(label)
    .execute(pool)
    .await?;

    let row: (String,) = sqlx::query_as(
        "SELECT created_at FROM merchant_api_keys WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(pool)
    .await?;

    tracing::info!(
        merchant_id,
        key_id = %id,
        key_type = key_type.as_str(),
        "API key created"
    );

    Ok(CreatedApiKey {
        id,
        key: raw,
        key_prefix,
        key_type,
        label: label.to_string(),
        created_at: row.0,
    })
}

pub async fn list_keys(
    pool: &SqlitePool,
    merchant_id: &str,
) -> anyhow::Result<Vec<ApiKeySummary>> {
    let rows: Vec<(String, String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, label, key_prefix, key_type, created_at, last_used_at
         FROM merchant_api_keys
         WHERE merchant_id = ? AND revoked_at IS NULL
         ORDER BY created_at DESC",
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, label, key_prefix, key_type, created_at, last_used_at)| ApiKeySummary {
            id,
            label,
            key_prefix,
            key_type: if key_type == "restricted" {
                KeyType::Restricted
            } else {
                KeyType::Full
            },
            created_at,
            last_used_at,
        })
        .collect())
}

/// Result of a revoke attempt.
#[derive(Debug)]
pub enum RevokeOutcome {
    /// Key revoked successfully.
    Revoked,
    /// Key not found, doesn't belong to this merchant, or already revoked.
    NotFound,
    /// Refused: this is the merchant's only remaining full-access credential.
    /// The dashboard token can still log in, but the API would be locked out.
    LastFullKey,
}

pub async fn revoke_key(
    pool: &SqlitePool,
    merchant_id: &str,
    key_id: &str,
) -> anyhow::Result<RevokeOutcome> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT key_type, COALESCE(revoked_at, '') FROM merchant_api_keys
         WHERE id = ? AND merchant_id = ?",
    )
    .bind(key_id)
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    let (key_type, revoked_at) = match row {
        Some(r) => r,
        None => return Ok(RevokeOutcome::NotFound),
    };
    if !revoked_at.is_empty() {
        return Ok(RevokeOutcome::NotFound);
    }

    // Block revoking the last full-access credential to avoid an account lockout
    // where the merchant can only reach the API via dashboard token.
    if key_type == "full" {
        let other_full: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM merchant_api_keys
             WHERE merchant_id = ?
               AND key_type = 'full'
               AND revoked_at IS NULL
               AND id != ?",
        )
        .bind(merchant_id)
        .bind(key_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        // We also check the legacy api_key_hash on the merchants row — if that
        // exists, the merchant still has one full-access credential, so we can
        // safely revoke this one.
        let legacy_hash: Option<String> = sqlx::query_scalar(
            "SELECT api_key_hash FROM merchants WHERE id = ? AND api_key_hash != ''",
        )
        .bind(merchant_id)
        .fetch_optional(pool)
        .await?;

        if other_full == 0 && legacy_hash.is_none() {
            return Ok(RevokeOutcome::LastFullKey);
        }
    }

    sqlx::query(
        "UPDATE merchant_api_keys
         SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ? AND merchant_id = ?",
    )
    .bind(key_id)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id, key_id, "API key revoked");
    Ok(RevokeOutcome::Revoked)
}
