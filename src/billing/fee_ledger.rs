use chrono::Utc;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;

/// Returns the effective fee rate for a merchant (per-merchant override or global default).
pub async fn get_effective_fee_rate(
    pool: &SqlitePool,
    merchant_id: &str,
    config: &Config,
) -> anyhow::Result<f64> {
    let merchant_rate: Option<f64> =
        sqlx::query_scalar("SELECT fee_rate FROM merchants WHERE id = ?")
            .bind(merchant_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(merchant_rate.unwrap_or(config.fee_rate))
}

pub async fn create_fee_entry(
    pool: &SqlitePool,
    invoice_id: &str,
    merchant_id: &str,
    fee_amount_zec: f64,
    fee_rate_applied: f64,
) -> anyhow::Result<()> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let fee_amount_zatoshis = crate::invoices::zec_to_zatoshis(fee_amount_zec)?;

    let cycle_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM billing_cycles WHERE merchant_id = ? AND status = 'open' LIMIT 1",
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    sqlx::query(
        "INSERT OR IGNORE INTO fee_ledger (id, invoice_id, merchant_id, fee_amount_zec, fee_amount_zatoshis, fee_rate_applied, billing_cycle_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(invoice_id)
    .bind(merchant_id)
    .bind(fee_amount_zec)
    .bind(fee_amount_zatoshis)
    .bind(fee_rate_applied)
    .bind(&cycle_id)
    .bind(&now)
    .execute(pool)
    .await?;

    if let Some(cid) = &cycle_id {
        sqlx::query(
            "UPDATE billing_cycles SET
                total_fees_zatoshis = total_fees_zatoshis + ?,
                outstanding_zatoshis = outstanding_zatoshis + ?,
                total_fees_zec = (total_fees_zatoshis + ?) / 100000000.0,
                outstanding_zec = (outstanding_zatoshis + ?) / 100000000.0
             WHERE id = ?",
        )
        .bind(fee_amount_zatoshis)
        .bind(fee_amount_zatoshis)
        .bind(fee_amount_zatoshis)
        .bind(fee_amount_zatoshis)
        .bind(cid)
        .execute(pool)
        .await?;
    }

    tracing::debug!(invoice_id, fee_amount_zec, "Fee entry created");
    Ok(())
}

pub async fn mark_fee_collected(pool: &SqlitePool, invoice_id: &str) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let result = sqlx::query(
        "UPDATE fee_ledger SET auto_collected = 1, collected_at = ?
         WHERE invoice_id = ? AND auto_collected = 0",
    )
    .bind(&now)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        let entry: Option<(i64, Option<String>)> = sqlx::query_as(
            "SELECT fee_amount_zatoshis, billing_cycle_id FROM fee_ledger WHERE invoice_id = ?",
        )
        .bind(invoice_id)
        .fetch_optional(pool)
        .await?;

        if let Some((amount, Some(cycle_id))) = entry {
            sqlx::query(
                "UPDATE billing_cycles SET
                    auto_collected_zatoshis = auto_collected_zatoshis + ?,
                    outstanding_zatoshis = MAX(0, outstanding_zatoshis - ?),
                    auto_collected_zec = (auto_collected_zatoshis + ?) / 100000000.0,
                    outstanding_zec = MAX(0, outstanding_zatoshis - ?) / 100000000.0
                 WHERE id = ?",
            )
            .bind(amount)
            .bind(amount)
            .bind(amount)
            .bind(amount)
            .bind(&cycle_id)
            .execute(pool)
            .await?;
        }

        tracing::info!(invoice_id, "Fee auto-collected");
    }

    Ok(())
}
