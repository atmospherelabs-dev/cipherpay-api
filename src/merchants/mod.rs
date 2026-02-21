use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Merchant {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub api_key_hash: String,
    #[serde(skip_serializing)]
    pub dashboard_token_hash: String,
    pub ufvk: String,
    pub payment_address: String,
    pub webhook_url: Option<String>,
    pub webhook_secret: String,
    pub recovery_email: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateMerchantRequest {
    pub name: Option<String>,
    pub ufvk: String,
    pub payment_address: String,
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
) -> anyhow::Result<CreateMerchantResponse> {
    let id = Uuid::new_v4().to_string();
    let api_key = generate_api_key();
    let key_hash = hash_key(&api_key);
    let dashboard_token = generate_dashboard_token();
    let dash_hash = hash_key(&dashboard_token);
    let webhook_secret = generate_webhook_secret();

    let name = req.name.as_deref().unwrap_or("").to_string();

    sqlx::query(
        "INSERT INTO merchants (id, name, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(&name)
    .bind(&key_hash)
    .bind(&dash_hash)
    .bind(&req.ufvk)
    .bind(&req.payment_address)
    .bind(&req.webhook_url)
    .bind(&webhook_secret)
    .bind(&req.email)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id = %id, "Merchant created");

    Ok(CreateMerchantResponse {
        merchant_id: id,
        api_key,
        dashboard_token,
        webhook_secret,
    })
}

type MerchantRow = (String, String, String, String, String, String, Option<String>, String, Option<String>, String);

const MERCHANT_COLS: &str = "id, name, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at";

fn row_to_merchant(r: MerchantRow) -> Merchant {
    Merchant {
        id: r.0, name: r.1, api_key_hash: r.2, dashboard_token_hash: r.3,
        ufvk: r.4, payment_address: r.5, webhook_url: r.6,
        webhook_secret: r.7, recovery_email: r.8, created_at: r.9,
    }
}

pub async fn get_all_merchants(pool: &SqlitePool) -> anyhow::Result<Vec<Merchant>> {
    let rows = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants")
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_merchant).collect())
}

pub async fn authenticate(pool: &SqlitePool, api_key: &str) -> anyhow::Result<Option<Merchant>> {
    let key_hash = hash_key(api_key);

    let row = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE api_key_hash = ?")
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(row_to_merchant))
}

pub async fn authenticate_dashboard(pool: &SqlitePool, token: &str) -> anyhow::Result<Option<Merchant>> {
    let token_hash = hash_key(token);

    let row = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE dashboard_token_hash = ?")
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(row_to_merchant))
}

pub async fn get_by_session(pool: &SqlitePool, session_id: &str) -> anyhow::Result<Option<Merchant>> {
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

    Ok(row.map(row_to_merchant))
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

pub async fn regenerate_webhook_secret(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<String> {
    let new_secret = generate_webhook_secret();
    sqlx::query("UPDATE merchants SET webhook_secret = ? WHERE id = ?")
        .bind(&new_secret)
        .bind(merchant_id)
        .execute(pool)
        .await?;
    tracing::info!(merchant_id, "Webhook secret regenerated");
    Ok(new_secret)
}

pub async fn find_by_email(pool: &SqlitePool, email: &str) -> anyhow::Result<Option<Merchant>> {
    let row = sqlx::query_as::<_, MerchantRow>(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE recovery_email = ?")
    )
    .bind(email)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(row_to_merchant))
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
        None => return Ok(None),
    };

    let new_token = regenerate_dashboard_token(pool, &merchant_id).await?;

    sqlx::query("DELETE FROM recovery_tokens WHERE id = ?")
        .bind(&recovery_id)
        .execute(pool)
        .await?;

    tracing::info!(merchant_id = %merchant_id, "Account recovered via email token");
    Ok(Some(new_token))
}
