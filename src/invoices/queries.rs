use sqlx::SqlitePool;

use super::{Invoice, InvoiceStatus};

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
         WHERE i.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Look up an invoice by its memo code (e.g. CP-C6CDB775)
pub async fn get_invoice_by_memo(
    pool: &SqlitePool,
    memo_code: &str,
) -> anyhow::Result<Option<Invoice>> {
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
         WHERE i.memo_code = ?",
    )
    .bind(memo_code)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn get_invoice_status(
    pool: &SqlitePool,
    id: &str,
) -> anyhow::Result<Option<InvoiceStatus>> {
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
