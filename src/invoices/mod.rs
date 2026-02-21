pub mod matching;
pub mod pricing;

use base64::Engine;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Invoice {
    pub id: String,
    pub merchant_id: String,
    pub memo_code: String,
    pub product_name: Option<String>,
    pub size: Option<String>,
    pub price_eur: f64,
    pub price_zec: f64,
    pub zec_rate_at_creation: f64,
    pub payment_address: String,
    pub zcash_uri: String,
    pub merchant_name: Option<String>,
    pub shipping_alias: Option<String>,
    pub shipping_address: Option<String>,
    pub shipping_region: Option<String>,
    pub refund_address: Option<String>,
    pub status: String,
    pub detected_txid: Option<String>,
    pub detected_at: Option<String>,
    pub confirmed_at: Option<String>,
    pub shipped_at: Option<String>,
    pub expires_at: String,
    pub purge_after: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, FromRow)]
pub struct InvoiceStatus {
    #[sqlx(rename = "id")]
    pub invoice_id: String,
    pub status: String,
    pub detected_txid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    pub product_id: Option<String>,
    pub product_name: Option<String>,
    pub size: Option<String>,
    pub price_eur: f64,
    pub shipping_alias: Option<String>,
    pub shipping_address: Option<String>,
    pub shipping_region: Option<String>,
    pub refund_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateInvoiceResponse {
    pub invoice_id: String,
    pub memo_code: String,
    pub price_eur: f64,
    pub price_zec: f64,
    pub zec_rate: f64,
    pub payment_address: String,
    pub zcash_uri: String,
    pub expires_at: String,
}

fn generate_memo_code() -> String {
    let bytes: [u8; 4] = rand::random();
    format!("CP-{}", hex::encode(bytes).to_uppercase())
}

pub async fn create_invoice(
    pool: &SqlitePool,
    merchant_id: &str,
    payment_address: &str,
    req: &CreateInvoiceRequest,
    zec_rate: f64,
    expiry_minutes: i64,
) -> anyhow::Result<CreateInvoiceResponse> {
    let id = Uuid::new_v4().to_string();
    let memo_code = generate_memo_code();
    let price_zec = req.price_eur / zec_rate;
    let expires_at = (Utc::now() + Duration::minutes(expiry_minutes))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let created_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(memo_code.as_bytes());
    let zcash_uri = format!(
        "zcash:{}?amount={:.8}&memo={}",
        payment_address, price_zec, memo_b64
    );

    sqlx::query(
        "INSERT INTO invoices (id, merchant_id, memo_code, product_id, product_name, size,
         price_eur, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         shipping_alias, shipping_address,
         shipping_region, refund_address, status, expires_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&memo_code)
    .bind(&req.product_id)
    .bind(&req.product_name)
    .bind(&req.size)
    .bind(req.price_eur)
    .bind(price_zec)
    .bind(zec_rate)
    .bind(payment_address)
    .bind(&zcash_uri)
    .bind(&req.shipping_alias)
    .bind(&req.shipping_address)
    .bind(&req.shipping_region)
    .bind(&req.refund_address)
    .bind(&expires_at)
    .bind(&created_at)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id = %id, memo = %memo_code, "Invoice created");

    Ok(CreateInvoiceResponse {
        invoice_id: id,
        memo_code,
        price_eur: req.price_eur,
        price_zec,
        zec_rate,
        payment_address: payment_address.to_string(),
        zcash_uri,
        expires_at,
    })
}

pub async fn get_invoice(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Invoice>> {
    let row = sqlx::query_as::<_, Invoice>(
        "SELECT i.id, i.merchant_id, i.memo_code, i.product_name, i.size,
         i.price_eur, i.price_zec, i.zec_rate_at_creation,
         COALESCE(NULLIF(i.payment_address, ''), m.payment_address) AS payment_address,
         i.zcash_uri,
         NULLIF(m.name, '') AS merchant_name,
         i.shipping_alias, i.shipping_address,
         i.shipping_region, i.refund_address, i.status, i.detected_txid, i.detected_at,
         i.confirmed_at, i.shipped_at, i.expires_at, i.purge_after, i.created_at
         FROM invoices i
         LEFT JOIN merchants m ON m.id = i.merchant_id
         WHERE i.id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Look up an invoice by its memo code (e.g. CP-C6CDB775)
pub async fn get_invoice_by_memo(pool: &SqlitePool, memo_code: &str) -> anyhow::Result<Option<Invoice>> {
    let row = sqlx::query_as::<_, Invoice>(
        "SELECT i.id, i.merchant_id, i.memo_code, i.product_name, i.size,
         i.price_eur, i.price_zec, i.zec_rate_at_creation,
         COALESCE(NULLIF(i.payment_address, ''), m.payment_address) AS payment_address,
         i.zcash_uri,
         NULLIF(m.name, '') AS merchant_name,
         i.shipping_alias, i.shipping_address,
         i.shipping_region, i.refund_address, i.status, i.detected_txid, i.detected_at,
         i.confirmed_at, i.shipped_at, i.expires_at, i.purge_after, i.created_at
         FROM invoices i
         LEFT JOIN merchants m ON m.id = i.merchant_id
         WHERE i.memo_code = ?"
    )
    .bind(memo_code)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn get_invoice_status(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<InvoiceStatus>> {
    let row = sqlx::query_as::<_, InvoiceStatus>(
        "SELECT id, status, detected_txid FROM invoices WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn get_pending_invoices(pool: &SqlitePool) -> anyhow::Result<Vec<Invoice>> {
    let rows = sqlx::query_as::<_, Invoice>(
        "SELECT id, merchant_id, memo_code, product_name, size,
         price_eur, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         NULL AS merchant_name,
         shipping_alias, shipping_address,
         shipping_region, refund_address, status, detected_txid, detected_at,
         confirmed_at, shipped_at, expires_at, purge_after, created_at
         FROM invoices WHERE status IN ('pending', 'detected')
         AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn mark_detected(pool: &SqlitePool, invoice_id: &str, txid: &str) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "UPDATE invoices SET status = 'detected', detected_txid = ?, detected_at = ?
         WHERE id = ? AND status = 'pending'"
    )
    .bind(txid)
    .bind(&now)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id, txid, "Payment detected");
    Ok(())
}

pub async fn mark_confirmed(pool: &SqlitePool, invoice_id: &str) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "UPDATE invoices SET status = 'confirmed', confirmed_at = ?
         WHERE id = ? AND status = 'detected'"
    )
    .bind(&now)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id, "Payment confirmed");
    Ok(())
}

pub async fn mark_shipped(pool: &SqlitePool, invoice_id: &str) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "UPDATE invoices SET status = 'shipped', shipped_at = ?
         WHERE id = ? AND status = 'confirmed'"
    )
    .bind(&now)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id, "Invoice marked as shipped");
    Ok(())
}

pub async fn mark_expired(pool: &SqlitePool, invoice_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE invoices SET status = 'expired'
         WHERE id = ? AND status = 'pending'"
    )
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id, "Invoice cancelled/expired");
    Ok(())
}

pub async fn expire_old_invoices(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "UPDATE invoices SET status = 'expired'
         WHERE status = 'pending' AND expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .execute(pool)
    .await?;

    let count = result.rows_affected();
    if count > 0 {
        tracing::info!(count, "Expired old invoices");
    }
    Ok(count)
}

pub async fn purge_old_data(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "UPDATE invoices SET shipping_alias = NULL, shipping_address = NULL
         WHERE purge_after IS NOT NULL
         AND purge_after < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         AND shipping_address IS NOT NULL"
    )
    .execute(pool)
    .await?;

    let count = result.rows_affected();
    if count > 0 {
        tracing::info!(count, "Purged shipping data");
    }
    Ok(count)
}
