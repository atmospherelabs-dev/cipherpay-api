use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Merchant {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub api_key_hash: String,
    #[serde(skip_serializing)]
    pub dashboard_token_hash: String,
    /// Stored as UIVK (incoming viewing key only). DB column name is legacy "ufvk".
    #[serde(skip_serializing)]
    pub ufvk: String,
    pub payment_address: String,
    pub webhook_url: Option<String>,
    pub webhook_secret: String,
    pub recovery_email: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing)]
    pub diversifier_index: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateMerchantRequest {
    pub name: Option<String>,
    pub ufvk: String,
    pub webhook_url: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateMerchantResponse {
    pub merchant_id: String,
    pub api_key: String,
    pub dashboard_token: String,
    pub webhook_secret: String,
}

fn generate_api_key() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("cpay_sk_{}", hex::encode(bytes))
}

fn generate_dashboard_token() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("cpay_dash_{}", hex::encode(bytes))
}

fn generate_webhook_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("whsec_{}", hex::encode(bytes))
}

pub fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

pub async fn create_merchant(
    pool: &SqlitePool,
    req: &CreateMerchantRequest,
    encryption_key: &str,
) -> anyhow::Result<CreateMerchantResponse> {
    let is_uivk_input = req.ufvk.starts_with("uivk");
    let viewing_key = if is_uivk_input {
        tracing::info!("UIVK received directly for new merchant registration");
        req.ufvk.clone()
    } else {
        let uivk = crate::scanner::decrypt::derive_uivk_from_ufvk(&req.ufvk)
            .map_err(|e| anyhow::anyhow!("Invalid viewing key — could not derive UIVK: {}", e))?;
        tracing::info!("UFVK received, derived UIVK for storage");
        uivk
    };

    let derived = crate::addresses::derive_invoice_address(&viewing_key, 0)
        .map_err(|e| anyhow::anyhow!("Invalid viewing key — could not derive address: {}", e))?;
    let payment_address = derived.ua_string;

    let id = Uuid::new_v4().to_string();
    let api_key = generate_api_key();
    let key_hash = hash_key(&api_key);
    let dashboard_token = generate_dashboard_token();
    let dash_hash = hash_key(&dashboard_token);
    let webhook_secret = generate_webhook_secret();

    let name = req.name.as_deref().unwrap_or("").to_string();

    // Store only the UIVK (never the full UFVK)
    let stored_ufvk = if encryption_key.is_empty() {
        viewing_key.clone()
    } else {
        crate::crypto::encrypt(&viewing_key, encryption_key)?
    };

    let stored_webhook_secret = if encryption_key.is_empty() {
        webhook_secret.clone()
    } else {
        crate::crypto::encrypt(&webhook_secret, encryption_key)?
    };

    let (stored_email, email_hash) = match &req.email {
        Some(email) if !email.is_empty() => {
            let encrypted = if encryption_key.is_empty() {
                email.clone()
            } else {
                crate::crypto::encrypt(email, encryption_key)?
            };
            let hash = crate::crypto::blind_index(email);
            (Some(encrypted), Some(hash))
        }
        _ => (None, None),
    };

    sqlx::query(
        "INSERT INTO merchants (id, name, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, recovery_email_hash, diversifier_index)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)"
    )
    .bind(&id)
    .bind(&name)
    .bind(&key_hash)
    .bind(&dash_hash)
    .bind(&stored_ufvk)
    .bind(&payment_address)
    .bind(&req.webhook_url)
    .bind(&stored_webhook_secret)
    .bind(&stored_email)
    .bind(&email_hash)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id = %id, key_type = if is_uivk_input { "UIVK" } else { "UFVK→UIVK" }, "Merchant created with derived address");

    Ok(CreateMerchantResponse {
        merchant_id: id,
        api_key,
        dashboard_token,
        webhook_secret,
    })
}

type MerchantRow = (String, String, String, String, String, String, Option<String>, String, Option<String>, String, i64);

const MERCHANT_COLS: &str = "id, name, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at, diversifier_index";

fn row_to_merchant(r: MerchantRow, encryption_key: &str) -> Merchant {
    let ufvk = crate::crypto::decrypt_or_plaintext(&r.4, encryption_key)
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "Failed to decrypt UFVK, using raw value");
            r.4.clone()
        });
    let webhook_secret = crate::crypto::decrypt_webhook_secret(&r.7, encryption_key)
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "Failed to decrypt webhook secret, using raw value");
            r.7.clone()
        });
    let recovery_email = r.8.as_deref()
        .map(|e| crate::crypto::decrypt_email(e, encryption_key).unwrap_or_else(|err| {
            tracing::error!(error = %err, "Failed to decrypt recovery email");
            e.to_string()
        }));
    Merchant {
        id: r.0, name: r.1, api_key_hash: r.2, dashboard_token_hash: r.3,
        ufvk, payment_address: r.5, webhook_url: r.6,
        webhook_secret, recovery_email, created_at: r.9,
        diversifier_index: r.10,
    }
}

pub async fn get_all_merchants(pool: &SqlitePool, encryption_key: &str) -> anyhow::Result<Vec<Merchant>> {
    let rows = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants")
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| row_to_merchant(r, encryption_key)).collect())
}

pub async fn get_merchant_by_id(pool: &SqlitePool, id: &str, encryption_key: &str) -> anyhow::Result<Option<Merchant>> {
    let row = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE id = ?")
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_merchant(r, encryption_key)))
}

pub async fn authenticate(pool: &SqlitePool, api_key: &str, encryption_key: &str) -> anyhow::Result<Option<Merchant>> {
    let key_hash = hash_key(api_key);

    let row = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE api_key_hash = ?")
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_merchant(r, encryption_key)))
}

pub async fn authenticate_dashboard(pool: &SqlitePool, token: &str, encryption_key: &str) -> anyhow::Result<Option<Merchant>> {
    let token_hash = hash_key(token);

    let row = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE dashboard_token_hash = ?")
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_merchant(r, encryption_key)))
}

pub async fn get_by_session(pool: &SqlitePool, session_id: &str, encryption_key: &str) -> anyhow::Result<Option<Merchant>> {
    let cols = MERCHANT_COLS.replace("id,", "m.id,").replace(", ", ", m.").replacen("m.id", "m.id", 1);
    let row = sqlx::query_as::<_, MerchantRow>(
        &format!(
            "SELECT {} FROM merchants m JOIN sessions s ON s.merchant_id = m.id
             WHERE s.id = ? AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            cols
        )
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_merchant(r, encryption_key)))
}

pub async fn regenerate_api_key(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<String> {
    let new_key = generate_api_key();
    let new_hash = hash_key(&new_key);
    sqlx::query("UPDATE merchants SET api_key_hash = ? WHERE id = ?")
        .bind(&new_hash)
        .bind(merchant_id)
        .execute(pool)
        .await?;
    tracing::info!(merchant_id, "API key regenerated");
    Ok(new_key)
}

pub async fn regenerate_dashboard_token(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<String> {
    let new_token = generate_dashboard_token();
    let new_hash = hash_key(&new_token);
    sqlx::query("UPDATE merchants SET dashboard_token_hash = ? WHERE id = ?")
        .bind(&new_hash)
        .bind(merchant_id)
        .execute(pool)
        .await?;

    // Invalidate ALL existing sessions for this merchant
    sqlx::query("DELETE FROM sessions WHERE merchant_id = ?")
        .bind(merchant_id)
        .execute(pool)
        .await?;

    tracing::info!(merchant_id, "Dashboard token regenerated, all sessions invalidated");
    Ok(new_token)
}

pub async fn regenerate_webhook_secret(pool: &SqlitePool, merchant_id: &str, encryption_key: &str) -> anyhow::Result<String> {
    let new_secret = generate_webhook_secret();
    let stored = if encryption_key.is_empty() {
        new_secret.clone()
    } else {
        crate::crypto::encrypt(&new_secret, encryption_key)?
    };
    sqlx::query("UPDATE merchants SET webhook_secret = ? WHERE id = ?")
        .bind(&stored)
        .bind(merchant_id)
        .execute(pool)
        .await?;
    tracing::info!(merchant_id, "Webhook secret regenerated");
    Ok(new_secret)
}

/// Atomically increment the merchant's diversifier_index and return the index to use.
/// The returned value is the index BEFORE the increment (i.e., the one to use for this invoice).
pub async fn next_diversifier_index(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<u32> {
    let row: (i64,) = sqlx::query_as(
        "UPDATE merchants SET diversifier_index = diversifier_index + 1 WHERE id = ? RETURNING diversifier_index - 1"
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;

    Ok(row.0 as u32)
}

pub async fn find_by_email(pool: &SqlitePool, email: &str, encryption_key: &str) -> anyhow::Result<Option<Merchant>> {
    let email_hash = crate::crypto::blind_index(email);
    let row = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE recovery_email_hash = ?")
    )
    .bind(&email_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_merchant(r, encryption_key)))
}

pub async fn create_recovery_token(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<String> {
    let token = Uuid::new_v4().to_string();
    let token_hash = hash_key(&token);
    let id = Uuid::new_v4().to_string();
    let expires_at = (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    sqlx::query("DELETE FROM recovery_tokens WHERE merchant_id = ?")
        .bind(merchant_id)
        .execute(pool)
        .await?;

    sqlx::query(
        "INSERT INTO recovery_tokens (id, merchant_id, token_hash, expires_at) VALUES (?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&token_hash)
    .bind(&expires_at)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id, "Recovery token created");
    Ok(token)
}

pub async fn has_outstanding_balance(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<bool> {
    let row: Option<(f64,)> = sqlx::query_as(
        "SELECT COALESCE(SUM(outstanding_zec), 0.0) FROM billing_cycles
         WHERE merchant_id = ? AND outstanding_zec > 0.0001"
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.0 > 0.0001).unwrap_or(false))
}

pub async fn delete_merchant(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE merchant_id = ?")
        .bind(merchant_id).execute(pool).await?;
    sqlx::query("DELETE FROM recovery_tokens WHERE merchant_id = ?")
        .bind(merchant_id).execute(pool).await?;
    sqlx::query("DELETE FROM fee_ledger WHERE merchant_id = ?")
        .bind(merchant_id).execute(pool).await?;
    sqlx::query("DELETE FROM billing_cycles WHERE merchant_id = ?")
        .bind(merchant_id).execute(pool).await?;
    sqlx::query("UPDATE products SET active = 0 WHERE merchant_id = ?")
        .bind(merchant_id).execute(pool).await?;
    sqlx::query("DELETE FROM merchants WHERE id = ?")
        .bind(merchant_id).execute(pool).await?;

    tracing::info!(merchant_id, "Merchant account deleted");
    Ok(())
}

pub async fn confirm_recovery_token(pool: &SqlitePool, token: &str) -> anyhow::Result<Option<String>> {
    let token_hash = hash_key(token);

    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT id, merchant_id FROM recovery_tokens
         WHERE token_hash = ? AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;

    let (recovery_id, merchant_id) = match row {
        Some(r) => r,
        None => {
            let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM recovery_tokens")
                .fetch_one(pool).await?;
            tracing::warn!(
                token_hash_prefix = &token_hash[..8],
                total_tokens_in_db = total.0,
                "Recovery token not found or expired"
            );
            return Ok(None);
        }
    };

    let new_token = regenerate_dashboard_token(pool, &merchant_id).await?;

    sqlx::query("DELETE FROM recovery_tokens WHERE id = ?")
        .bind(&recovery_id)
        .execute(pool)
        .await?;

    tracing::info!(merchant_id = %merchant_id, "Account recovered via email token");
    Ok(Some(new_token))
}
