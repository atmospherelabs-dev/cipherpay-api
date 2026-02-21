use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Merchant {
    pub id: String,
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

    sqlx::query(
        "INSERT INTO merchants (id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
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

pub async fn get_all_merchants(pool: &SqlitePool) -> anyhow::Result<Vec<Merchant>> {
    let rows = sqlx::query_as::<_, (String, String, String, String, String, Option<String>, String, Option<String>, String)>(
        "SELECT id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at FROM merchants"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at)| {
        Merchant { id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at }
    }).collect())
}

/// Authenticate a merchant by API key (cpay_sk_...).
pub async fn authenticate(pool: &SqlitePool, api_key: &str) -> anyhow::Result<Option<Merchant>> {
    let key_hash = hash_key(api_key);

    let row = sqlx::query_as::<_, (String, String, String, String, String, Option<String>, String, Option<String>, String)>(
        "SELECT id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at
         FROM merchants WHERE api_key_hash = ?"
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at)| {
        Merchant { id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at }
    }))
}

/// Authenticate a merchant by dashboard token (cpay_dash_...).
pub async fn authenticate_dashboard(pool: &SqlitePool, token: &str) -> anyhow::Result<Option<Merchant>> {
    let token_hash = hash_key(token);

    let row = sqlx::query_as::<_, (String, String, String, String, String, Option<String>, String, Option<String>, String)>(
        "SELECT id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at
         FROM merchants WHERE dashboard_token_hash = ?"
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at)| {
        Merchant { id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at }
    }))
}

/// Look up a merchant by session ID (from the cpay_session cookie).
pub async fn get_by_session(pool: &SqlitePool, session_id: &str) -> anyhow::Result<Option<Merchant>> {
    let row = sqlx::query_as::<_, (String, String, String, String, String, Option<String>, String, Option<String>, String)>(
        "SELECT m.id, m.api_key_hash, m.dashboard_token_hash, m.ufvk, m.payment_address, m.webhook_url, m.webhook_secret, m.recovery_email, m.created_at
         FROM merchants m
         JOIN sessions s ON s.merchant_id = m.id
         WHERE s.id = ? AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at)| {
        Merchant { id, api_key_hash, dashboard_token_hash, ufvk, payment_address, webhook_url, webhook_secret, recovery_email, created_at }
    }))
}
