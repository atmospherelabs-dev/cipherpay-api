use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Merchant {
    pub id: String,
    pub api_key_hash: String,
    pub ufvk: String,
    pub payment_address: String,
    pub webhook_url: Option<String>,
    pub webhook_secret: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateMerchantRequest {
    pub ufvk: String,
    pub payment_address: String,
    pub webhook_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateMerchantResponse {
    pub merchant_id: String,
    pub api_key: String,
    pub webhook_secret: String,
}

fn generate_api_key() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("cpay_{}", hex::encode(bytes))
}

fn generate_webhook_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("whsec_{}", hex::encode(bytes))
}

fn hash_api_key(key: &str) -> String {
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
    let key_hash = hash_api_key(&api_key);
    let webhook_secret = generate_webhook_secret();

    sqlx::query(
        "INSERT INTO merchants (id, api_key_hash, ufvk, payment_address, webhook_url, webhook_secret)
         VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(&key_hash)
    .bind(&req.ufvk)
    .bind(&req.payment_address)
    .bind(&req.webhook_url)
    .bind(&webhook_secret)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id = %id, "Merchant created");

    Ok(CreateMerchantResponse {
        merchant_id: id,
        api_key,
        webhook_secret,
    })
}

pub async fn get_all_merchants(pool: &SqlitePool) -> anyhow::Result<Vec<Merchant>> {
    let rows = sqlx::query_as::<_, (String, String, String, String, Option<String>, String, String)>(
        "SELECT id, api_key_hash, ufvk, payment_address, webhook_url, webhook_secret, created_at FROM merchants"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(id, api_key_hash, ufvk, payment_address, webhook_url, webhook_secret, created_at)| {
        Merchant { id, api_key_hash, ufvk, payment_address, webhook_url, webhook_secret, created_at }
    }).collect())
}

/// Authenticate a merchant by API key. Hashes the provided key
/// and looks it up in the database.
pub async fn authenticate(pool: &SqlitePool, api_key: &str) -> anyhow::Result<Option<Merchant>> {
    let key_hash = hash_api_key(api_key);

    let row = sqlx::query_as::<_, (String, String, String, String, Option<String>, String, String)>(
        "SELECT id, api_key_hash, ufvk, payment_address, webhook_url, webhook_secret, created_at
         FROM merchants WHERE api_key_hash = ?"
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, api_key_hash, ufvk, payment_address, webhook_url, webhook_secret, created_at)| {
        Merchant { id, api_key_hash, ufvk, payment_address, webhook_url, webhook_secret, created_at }
    }))
}
