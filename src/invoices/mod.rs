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
    pub product_id: Option<String>,
    pub product_name: Option<String>,
    pub size: Option<String>,
    pub price_eur: f64,
    pub price_usd: Option<f64>,
    pub currency: Option<String>,
    pub price_zec: f64,
    pub zec_rate_at_creation: f64,
    pub amount: Option<f64>,
    pub price_id: Option<String>,
    pub payment_address: String,
    pub zcash_uri: String,
    pub merchant_name: Option<String>,
    pub refund_address: Option<String>,
    pub status: String,
    pub detected_txid: Option<String>,
    pub detected_at: Option<String>,
    pub confirmed_at: Option<String>,
    pub refunded_at: Option<String>,
    pub refund_txid: Option<String>,
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
    pub subscription_id: Option<String>,
    pub payment_link_id: Option<String>,
    pub is_donation: i32,
    pub campaign_counted: i32,
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
    pub price_id: Option<String>,
    pub product_name: Option<String>,
    pub size: Option<String>,
    #[serde(alias = "price_eur")]
    pub amount: f64,
    pub currency: Option<String>,
    pub refund_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateInvoiceResponse {
    pub invoice_id: String,
    pub memo_code: String,
    pub amount: f64,
    pub currency: String,
    pub price_eur: f64,
    pub price_usd: f64,
    pub price_zec: f64,
    pub zec_rate: f64,
    pub price_id: Option<String>,
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

/// Minimum fee output to include in ZIP 321 URIs (10,000 zatoshis = 0.0001 ZEC).
/// Below this, the output costs more to spend than it's worth. Fees on small
/// payments still accrue in the billing cycle and settle via the normal threshold.
const MIN_FEE_ZEC: f64 = 0.0001;

pub async fn create_invoice(
    pool: &SqlitePool,
    merchant_id: &str,
    merchant_ufvk: &str,
    req: &CreateInvoiceRequest,
    rates: &crate::invoices::pricing::ZecRates,
    expiry_minutes: i64,
    fee_config: Option<&FeeConfig>,
) -> anyhow::Result<CreateInvoiceResponse> {
    let id = Uuid::new_v4().to_string();
    let memo_code = generate_memo_code();
    let currency = req.currency.as_deref().unwrap_or("EUR");
    let amount = req.amount;

    let zec_rate = rates.rate_for_currency(currency)
        .ok_or_else(|| anyhow::anyhow!("Unsupported currency: {}", currency))?;

    if zec_rate <= 0.0 {
        anyhow::bail!("No exchange rate available for {}", currency);
    }

    let price_zec = amount / zec_rate;
    let price_eur = if currency == "EUR" { amount } else { price_zec * rates.zec_eur };
    let price_usd = if currency == "USD" { amount } else { price_zec * rates.zec_usd };

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
        if fee_amount >= MIN_FEE_ZEC {
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

    if let Some(price_id) = req.price_id.as_deref() {
        // Atomic capacity guard: prevent sold-out race windows for capped tiers.
        // This is a single guarded insert: if no row is inserted, the tier is sold out.
        let result = sqlx::query(
            "INSERT INTO invoices (
                id, merchant_id, memo_code, product_id, product_name, size,
                price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
                amount, price_id,
                payment_address, zcash_uri,
                refund_address, status, expires_at, created_at,
                diversifier_index, orchard_receiver_hex, price_zatoshis
             )
             SELECT
                ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?,
                ?, ?,
                ?, ?,
                ?, 'pending', ?, ?, ?, ?, ?
             WHERE (
                SELECT COUNT(*) FROM invoices i
                WHERE i.price_id = ?
                  AND i.status NOT IN ('expired', 'draft')
             ) < (
                SELECT COALESCE(p.max_quantity, 9223372036854775807)
                FROM prices p
                WHERE p.id = ?
             )"
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
        .bind(zec_rate)
        .bind(amount)
        .bind(price_id)
        .bind(payment_address)
        .bind(&zcash_uri)
        .bind(&req.refund_address)
        .bind(&expires_at)
        .bind(&created_at)
        .bind(div_index as i64)
        .bind(&derived.orchard_receiver_hex)
        .bind(price_zatoshis)
        .bind(price_id)
        .bind(price_id)
        .execute(pool)
        .await?;

        if result.rows_affected() == 0 {
            anyhow::bail!("Sold out");
        }
    } else {
        sqlx::query(
            "INSERT INTO invoices (id, merchant_id, memo_code, product_id, product_name, size,
             price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
             amount, price_id,
             payment_address, zcash_uri,
             refund_address, status, expires_at, created_at,
             diversifier_index, orchard_receiver_hex, price_zatoshis)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?)"
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
        .bind(zec_rate)
        .bind(amount)
        .bind(&req.price_id)
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
    }

    tracing::info!(
        invoice_id = %id,
        currency = %currency,
        diversifier_index = div_index,
        "Invoice created with unique address"
    );
    tracing::debug!(
        invoice_id = %id,
        memo = %memo_code,
        amount = %amount,
        "Invoice details"
    );

    Ok(CreateInvoiceResponse {
        invoice_id: id,
        memo_code,
        amount,
        currency: currency.to_string(),
        price_eur,
        price_usd,
        price_zec,
        zec_rate: zec_rate,
        price_id: req.price_id.clone(),
        payment_address: payment_address.to_string(),
        zcash_uri,
        expires_at,
    })
}

pub async fn get_invoice(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Invoice>> {
    let row = sqlx::query_as::<_, Invoice>(
        "SELECT i.id, i.merchant_id, i.memo_code, i.product_id, i.product_name, i.size,
         i.price_eur, i.price_usd, i.currency, i.price_zec, i.zec_rate_at_creation,
         i.amount, i.price_id,
         COALESCE(NULLIF(i.payment_address, ''), m.payment_address) AS payment_address,
         i.zcash_uri,
         NULLIF(m.name, '') AS merchant_name,
         i.refund_address, i.status, i.detected_txid, i.detected_at,
         i.confirmed_at, i.refunded_at, i.refund_txid, i.expires_at, i.purge_after, i.created_at,
         i.orchard_receiver_hex, i.diversifier_index,
         i.price_zatoshis, i.received_zatoshis,
         i.subscription_id,
         i.payment_link_id, i.is_donation, i.campaign_counted
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
        "SELECT i.id, i.merchant_id, i.memo_code, i.product_id, i.product_name, i.size,
         i.price_eur, i.price_usd, i.currency, i.price_zec, i.zec_rate_at_creation,
         i.amount, i.price_id,
         COALESCE(NULLIF(i.payment_address, ''), m.payment_address) AS payment_address,
         i.zcash_uri,
         NULLIF(m.name, '') AS merchant_name,
         i.refund_address, i.status, i.detected_txid, i.detected_at,
         i.confirmed_at, i.refunded_at, i.refund_txid, i.expires_at, i.purge_after, i.created_at,
         i.orchard_receiver_hex, i.diversifier_index,
         i.price_zatoshis, i.received_zatoshis,
         i.subscription_id,
         i.payment_link_id, i.is_donation, i.campaign_counted
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
        "SELECT id, merchant_id, memo_code, product_id, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
         amount, price_id,
         payment_address, zcash_uri,
         NULL AS merchant_name,
         refund_address, status, detected_txid, detected_at,
         confirmed_at, NULL AS refunded_at, NULL AS refund_txid, expires_at, purge_after, created_at,
         orchard_receiver_hex, diversifier_index,
         price_zatoshis, received_zatoshis,
         subscription_id,
         payment_link_id, is_donation, campaign_counted
         FROM invoices WHERE status IN ('pending', 'underpaid', 'detected')
         AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Returns true if the status actually changed (used to gate webhook dispatch).
pub async fn mark_detected(pool: &SqlitePool, invoice_id: &str, txid: &str, received_zatoshis: i64) -> anyhow::Result<bool> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let new_expires = (Utc::now() + Duration::minutes(30))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let result = sqlx::query(
        "UPDATE invoices SET status = 'detected', detected_txid = ?, detected_at = ?, received_zatoshis = ?, expires_at = ?
         WHERE id = ? AND status IN ('pending', 'underpaid')"
    )
    .bind(txid)
    .bind(&now)
    .bind(received_zatoshis)
    .bind(&new_expires)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    let changed = result.rows_affected() > 0;
    if changed {
        tracing::info!(invoice_id, txid, received_zatoshis, "Payment detected");
    }
    Ok(changed)
}

/// Returns true if the status actually changed (used to gate webhook dispatch).
pub async fn mark_confirmed(pool: &SqlitePool, invoice_id: &str) -> anyhow::Result<bool> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let result = sqlx::query(
        "UPDATE invoices SET status = 'confirmed', confirmed_at = ?
         WHERE id = ? AND status = 'detected'"
    )
    .bind(&now)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    let changed = result.rows_affected() > 0;
    if changed {
        tracing::info!(invoice_id, "Payment confirmed");
    }
    Ok(changed)
}

pub async fn mark_refunded(pool: &SqlitePool, invoice_id: &str, refund_txid: Option<&str>) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "UPDATE invoices SET status = 'refunded', refunded_at = ?, refund_txid = ?
         WHERE id = ? AND status = 'confirmed'"
    )
    .bind(&now)
    .bind(refund_txid)
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
/// Only operates on invoices in 'underpaid' status to prevent race conditions.
pub async fn accumulate_payment(pool: &SqlitePool, invoice_id: &str, additional_zatoshis: i64) -> anyhow::Result<i64> {
    let new_expires = (Utc::now() + Duration::minutes(10))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE invoices SET received_zatoshis = received_zatoshis + ?, expires_at = ?
         WHERE id = ? AND status = 'underpaid' RETURNING received_zatoshis"
    )
    .bind(additional_zatoshis)
    .bind(&new_expires)
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?;

    match row {
        Some((total,)) => {
            tracing::info!(invoice_id, additional_zatoshis, total, "Payment accumulated");
            Ok(total)
        }
        None => {
            tracing::warn!(invoice_id, "accumulate_payment: invoice not in underpaid status, skipping");
            anyhow::bail!("invoice not in underpaid status")
        }
    }
}

pub async fn update_refund_address(pool: &SqlitePool, invoice_id: &str, address: &str) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE invoices SET refund_address = ?
         WHERE id = ? AND status IN ('pending', 'underpaid', 'expired')
         AND (refund_address IS NULL OR refund_address = '')"
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

/// Create a draft invoice for a subscription renewal. No ZEC conversion yet;
/// the customer will finalize it (lock ZEC rate) when they open the payment page.
pub async fn create_draft_invoice(
    pool: &SqlitePool,
    merchant_id: &str,
    merchant_ufvk: &str,
    subscription_id: &str,
    product_name: Option<&str>,
    amount: f64,
    currency: &str,
    price_id: Option<&str>,
    expires_at: &str,
    fee_config: Option<&FeeConfig>,
) -> anyhow::Result<Invoice> {
    let id = Uuid::new_v4().to_string();
    let memo_code = generate_memo_code();
    let created_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let div_index = crate::merchants::next_diversifier_index(pool, merchant_id).await?;
    let derived = crate::addresses::derive_invoice_address(merchant_ufvk, div_index)?;
    let payment_address = &derived.ua_string;

    let _ = fee_config; // fee will be applied at finalization when ZEC amount is known

    sqlx::query(
        "INSERT INTO invoices (id, merchant_id, memo_code, product_name,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
         amount, price_id, subscription_id,
         payment_address, zcash_uri,
         refund_address, status, expires_at, created_at,
         diversifier_index, orchard_receiver_hex, price_zatoshis)
         VALUES (?, ?, ?, ?, 0.0, 0.0, ?, 0.0, 0.0, ?, ?, ?, ?, '', NULL, 'draft', ?, ?, ?, ?, 0)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&memo_code)
    .bind(product_name)
    .bind(currency)
    .bind(amount)
    .bind(price_id)
    .bind(subscription_id)
    .bind(payment_address)
    .bind(expires_at)
    .bind(&created_at)
    .bind(div_index as i64)
    .bind(&derived.orchard_receiver_hex)
    .execute(pool)
    .await?;

    tracing::info!(
        invoice_id = %id,
        subscription_id,
        currency,
        amount,
        "Draft invoice created for subscription"
    );

    get_invoice(pool, &id).await?
        .ok_or_else(|| anyhow::anyhow!("Draft invoice not found after insert"))
}

/// Finalize a draft (or re-finalize an expired) invoice: lock ZEC rate, start 15-min timer.
pub async fn finalize_invoice(
    pool: &SqlitePool,
    invoice_id: &str,
    rates: &crate::invoices::pricing::ZecRates,
    fee_config: Option<&FeeConfig>,
) -> anyhow::Result<Invoice> {
    let invoice = get_invoice(pool, invoice_id).await?
        .ok_or_else(|| anyhow::anyhow!("Invoice not found"))?;

    if invoice.status != "draft" && invoice.status != "expired" {
        anyhow::bail!("Invoice must be in draft or expired status to finalize (current: {})", invoice.status);
    }

    // In-flight payment guard: prevent re-finalization if payment already detected
    if invoice.received_zatoshis > 0 || invoice.detected_txid.is_some() {
        anyhow::bail!("Payment already detected for this invoice, awaiting confirmation");
    }

    // Period guard for subscription invoices
    if let Some(ref sub_id) = invoice.subscription_id {
        let sub = crate::subscriptions::get_subscription(pool, sub_id).await?;
        if let Some(s) = sub {
            let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            if now > s.current_period_end {
                anyhow::bail!("Subscription billing period has ended");
            }
        }
    }

    let currency = invoice.currency.as_deref().unwrap_or("EUR");
    let amount = invoice.amount.unwrap_or(invoice.price_eur);

    let zec_rate = rates.rate_for_currency(currency)
        .ok_or_else(|| anyhow::anyhow!("Unsupported currency: {}", currency))?;

    if zec_rate <= 0.0 {
        anyhow::bail!("No exchange rate available for {}", currency);
    }

    let price_zec = amount / zec_rate;
    let price_eur = if currency == "EUR" { amount } else { price_zec * rates.zec_eur };
    let price_usd = if currency == "USD" { amount } else { price_zec * rates.zec_usd };
    let price_zatoshis = (price_zec * 100_000_000.0) as i64;

    let expires_at = (Utc::now() + Duration::minutes(15))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(invoice.memo_code.as_bytes());

    let zcash_uri = if let Some(fc) = fee_config {
        let fee_amount = price_zec * fc.fee_rate;
        if fee_amount >= MIN_FEE_ZEC {
            let fee_memo = format!("FEE-{}", invoice.id);
            let fee_memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(fee_memo.as_bytes());
            format!(
                "zcash:?address={}&amount={:.8}&memo={}&address.1={}&amount.1={:.8}&memo.1={}",
                invoice.payment_address, price_zec, memo_b64,
                fc.fee_address, fee_amount, fee_memo_b64
            )
        } else {
            format!("zcash:{}?amount={:.8}&memo={}", invoice.payment_address, price_zec, memo_b64)
        }
    } else {
        format!("zcash:{}?amount={:.8}&memo={}", invoice.payment_address, price_zec, memo_b64)
    };

    sqlx::query(
        "UPDATE invoices SET status = 'pending',
         price_zec = ?, price_eur = ?, price_usd = ?,
         zec_rate_at_creation = ?, price_zatoshis = ?,
         zcash_uri = ?, expires_at = ?
         WHERE id = ?"
    )
    .bind(price_zec)
    .bind(price_eur)
    .bind(price_usd)
    .bind(zec_rate)
    .bind(price_zatoshis)
    .bind(&zcash_uri)
    .bind(&expires_at)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    tracing::info!(
        invoice_id,
        price_zec,
        zec_rate,
        "Invoice finalized (ZEC rate locked)"
    );

    get_invoice(pool, invoice_id).await?
        .ok_or_else(|| anyhow::anyhow!("Invoice not found after finalization"))
}

