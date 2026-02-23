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
    pub price_usd: Option<f64>,
    pub currency: Option<String>,
    pub price_zec: f64,
    pub zec_rate_at_creation: f64,
    pub payment_address: String,
    pub zcash_uri: String,
    pub merchant_name: Option<String>,
    pub refund_address: Option<String>,
    pub status: String,
    pub detected_txid: Option<String>,
    pub detected_at: Option<String>,
    pub confirmed_at: Option<String>,
    pub refunded_at: Option<String>,
    pub expires_at: String,
    pub purge_after: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing)]
    pub orchard_receiver_hex: Option<String>,
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    pub diversifier_index: Option<i64>,
    pub price_zatoshis: i64,
    pub received_zatoshis: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct InvoiceStatus {
    #[sqlx(rename = "id")]
    pub invoice_id: String,
    pub status: String,
    pub detected_txid: Option<String>,
    pub received_zatoshis: i64,
    pub price_zatoshis: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    pub product_id: Option<String>,
    pub product_name: Option<String>,
    pub size: Option<String>,
    pub price_eur: f64,
    pub currency: Option<String>,
    pub refund_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateInvoiceResponse {
    pub invoice_id: String,
    pub memo_code: String,
    pub price_eur: f64,
    pub price_usd: f64,
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

pub struct FeeConfig {
    pub fee_address: String,
    pub fee_rate: f64,
}

pub async fn create_invoice(
    pool: &SqlitePool,
    merchant_id: &str,
    merchant_ufvk: &str,
    req: &CreateInvoiceRequest,
    zec_eur: f64,
    zec_usd: f64,
    expiry_minutes: i64,
    fee_config: Option<&FeeConfig>,
) -> anyhow::Result<CreateInvoiceResponse> {
    let id = Uuid::new_v4().to_string();
    let memo_code = generate_memo_code();
    let currency = req.currency.as_deref().unwrap_or("EUR");
    let (price_eur, price_usd, price_zec) = if currency == "USD" {
        let usd = req.price_eur;
        let zec = usd / zec_usd;
        let eur = zec * zec_eur;
        (eur, usd, zec)
    } else {
        let zec = req.price_eur / zec_eur;
        let usd = zec * zec_usd;
        (req.price_eur, usd, zec)
    };
    let expires_at = (Utc::now() + Duration::minutes(expiry_minutes))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let created_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let div_index = crate::merchants::next_diversifier_index(pool, merchant_id).await?;
    let derived = crate::addresses::derive_invoice_address(merchant_ufvk, div_index)?;
    let payment_address = &derived.ua_string;

    let memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(memo_code.as_bytes());

    let zcash_uri = if let Some(fc) = fee_config {
        let fee_amount = price_zec * fc.fee_rate;
        if fee_amount >= 0.00000001 {
            let fee_memo = format!("FEE-{}", id);
            let fee_memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(fee_memo.as_bytes());
            format!(
                "zcash:?address={}&amount={:.8}&memo={}&address.1={}&amount.1={:.8}&memo.1={}",
                payment_address, price_zec, memo_b64,
                fc.fee_address, fee_amount, fee_memo_b64
            )
        } else {
            format!("zcash:{}?amount={:.8}&memo={}", payment_address, price_zec, memo_b64)
        }
    } else {
        format!("zcash:{}?amount={:.8}&memo={}", payment_address, price_zec, memo_b64)
    };

    let price_zatoshis = (price_zec * 100_000_000.0) as i64;

    sqlx::query(
        "INSERT INTO invoices (id, merchant_id, memo_code, product_id, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         refund_address, status, expires_at, created_at,
         diversifier_index, orchard_receiver_hex, price_zatoshis)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&memo_code)
    .bind(&req.product_id)
    .bind(&req.product_name)
    .bind(&req.size)
    .bind(price_eur)
    .bind(price_usd)
    .bind(currency)
    .bind(price_zec)
    .bind(zec_eur)
    .bind(payment_address)
    .bind(&zcash_uri)
    .bind(&req.refund_address)
    .bind(&expires_at)
    .bind(&created_at)
    .bind(div_index as i64)
    .bind(&derived.orchard_receiver_hex)
    .bind(price_zatoshis)
    .execute(pool)
    .await?;

    tracing::info!(
        invoice_id = %id,
        memo = %memo_code,
        diversifier_index = div_index,
        "Invoice created with unique address"
    );

    Ok(CreateInvoiceResponse {
        invoice_id: id,
        memo_code,
        price_eur,
        price_usd,
        price_zec,
        zec_rate: zec_eur,
        payment_address: payment_address.to_string(),
        zcash_uri,
        expires_at,
    })
}

pub async fn get_invoice(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Invoice>> {
    let row = sqlx::query_as::<_, Invoice>(
        "SELECT i.id, i.merchant_id, i.memo_code, i.product_name, i.size,
         i.price_eur, i.price_usd, i.currency, i.price_zec, i.zec_rate_at_creation,
         COALESCE(NULLIF(i.payment_address, ''), m.payment_address) AS payment_address,
         i.zcash_uri,
         NULLIF(m.name, '') AS merchant_name,
         i.refund_address, i.status, i.detected_txid, i.detected_at,
         i.confirmed_at, i.refunded_at, i.expires_at, i.purge_after, i.created_at,
         i.orchard_receiver_hex, i.diversifier_index,
         i.price_zatoshis, i.received_zatoshis
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
         i.price_eur, i.price_usd, i.currency, i.price_zec, i.zec_rate_at_creation,
         COALESCE(NULLIF(i.payment_address, ''), m.payment_address) AS payment_address,
         i.zcash_uri,
         NULLIF(m.name, '') AS merchant_name,
         i.refund_address, i.status, i.detected_txid, i.detected_at,
         i.confirmed_at, i.refunded_at, i.expires_at, i.purge_after, i.created_at,
         i.orchard_receiver_hex, i.diversifier_index,
         i.price_zatoshis, i.received_zatoshis
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
        "SELECT id, status, detected_txid, received_zatoshis, price_zatoshis FROM invoices WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn get_pending_invoices(pool: &SqlitePool) -> anyhow::Result<Vec<Invoice>> {
    let rows = sqlx::query_as::<_, Invoice>(
        "SELECT id, merchant_id, memo_code, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         NULL AS merchant_name,
         refund_address, status, detected_txid, detected_at,
         confirmed_at, NULL AS refunded_at, expires_at, purge_after, created_at,
         orchard_receiver_hex, diversifier_index,
         price_zatoshis, received_zatoshis
         FROM invoices WHERE status IN ('pending', 'underpaid', 'detected')
         AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Find a pending invoice by its Orchard receiver hex (O(1) indexed lookup).
pub async fn find_by_orchard_receiver(pool: &SqlitePool, receiver_hex: &str) -> anyhow::Result<Option<Invoice>> {
    let row = sqlx::query_as::<_, Invoice>(
        "SELECT id, merchant_id, memo_code, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         NULL AS merchant_name,
         refund_address, status, detected_txid, detected_at,
         confirmed_at, NULL AS refunded_at, expires_at, purge_after, created_at,
         orchard_receiver_hex, diversifier_index,
         price_zatoshis, received_zatoshis
         FROM invoices WHERE orchard_receiver_hex = ? AND status IN ('pending', 'underpaid', 'detected')
         AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .bind(receiver_hex)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn mark_detected(pool: &SqlitePool, invoice_id: &str, txid: &str, received_zatoshis: i64) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "UPDATE invoices SET status = 'detected', detected_txid = ?, detected_at = ?, received_zatoshis = ?
         WHERE id = ? AND status IN ('pending', 'underpaid')"
    )
    .bind(txid)
    .bind(&now)
    .bind(received_zatoshis)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id, txid, received_zatoshis, "Payment detected");
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

pub async fn mark_refunded(pool: &SqlitePool, invoice_id: &str) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "UPDATE invoices SET status = 'refunded', refunded_at = ?
         WHERE id = ? AND status = 'confirmed'"
    )
    .bind(&now)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id, "Invoice marked as refunded");
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
         WHERE status IN ('pending', 'underpaid') AND expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .execute(pool)
    .await?;

    let count = result.rows_affected();
    if count > 0 {
        tracing::info!(count, "Expired old invoices");
    }
    Ok(count)
}

pub async fn mark_underpaid(pool: &SqlitePool, invoice_id: &str, received_zatoshis: i64, txid: &str) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let new_expires = (Utc::now() + Duration::minutes(10))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    sqlx::query(
        "UPDATE invoices SET status = 'underpaid', received_zatoshis = ?, detected_txid = ?,
         detected_at = ?, expires_at = ?
         WHERE id = ? AND status = 'pending'"
    )
    .bind(received_zatoshis)
    .bind(txid)
    .bind(&now)
    .bind(&new_expires)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(invoice_id, received_zatoshis, "Invoice marked as underpaid");
    Ok(())
}

/// Add additional zatoshis to an underpaid invoice and extend its expiry.
/// Returns the new total received_zatoshis.
pub async fn accumulate_payment(pool: &SqlitePool, invoice_id: &str, additional_zatoshis: i64) -> anyhow::Result<i64> {
    let new_expires = (Utc::now() + Duration::minutes(10))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let row: (i64,) = sqlx::query_as(
        "UPDATE invoices SET received_zatoshis = received_zatoshis + ?, expires_at = ?
         WHERE id = ? RETURNING received_zatoshis"
    )
    .bind(additional_zatoshis)
    .bind(&new_expires)
    .bind(invoice_id)
    .fetch_one(pool)
    .await?;

    tracing::info!(invoice_id, additional_zatoshis, total = row.0, "Payment accumulated");
    Ok(row.0)
}

pub async fn update_refund_address(pool: &SqlitePool, invoice_id: &str, address: &str) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE invoices SET refund_address = ?
         WHERE id = ? AND status IN ('pending', 'underpaid', 'expired')"
    )
    .bind(address)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub fn zatoshis_to_zec(z: i64) -> f64 {
    format!("{:.8}", z as f64 / 100_000_000.0).parse::<f64>().unwrap_or(0.0)
}

