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
    let discounted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM merchant_referrals WHERE merchant_id=? AND discount_until>strftime('%Y-%m-%dT%H:%M:%SZ','now'))")
        .bind(merchant_id).fetch_one(pool).await?;
    let base = merchant_rate.unwrap_or(config.fee_rate);
    Ok(if discounted { base.min(0.005) } else { base })
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

    // The entry and aggregate must commit together. A duplicate scanner event
    // must not increase the cycle balance when its invoice entry already exists.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let settled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM crosschain_settlements WHERE invoice_id=?)",
    )
    .bind(invoice_id)
    .fetch_one(&mut *tx)
    .await?;
    if settled {
        return Ok(());
    }
    let unresolved: bool = sqlx::query_scalar("SELECT (EXISTS(SELECT 1 FROM crosschain_attempts WHERE invoice_id=?) OR EXISTS(SELECT 1 FROM crosschain_fees WHERE invoice_id=?)) AND NOT EXISTS(SELECT 1 FROM deferred_payment_fees WHERE invoice_id=? AND state='direct')")
        .bind(invoice_id).bind(invoice_id).bind(invoice_id).fetch_one(&mut *tx).await?;
    if unresolved {
        // Preserve the obligation, but do not bill during provider outages or
        // before we can distinguish a direct transfer from a delayed swap.
        sqlx::query("INSERT INTO deferred_payment_fees(invoice_id,merchant_id,amount_zec,rate) VALUES (?,?,?,?) ON CONFLICT(invoice_id) DO NOTHING")
            .bind(invoice_id).bind(merchant_id).bind(fee_amount_zec).bind(fee_rate_applied).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let inserted: Option<(Option<String>,)> = sqlx::query_as(
        "INSERT INTO fee_ledger (id, invoice_id, merchant_id, fee_amount_zec, fee_amount_zatoshis, fee_rate_applied, billing_cycle_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?,
             (SELECT id FROM billing_cycles WHERE merchant_id = ? AND status = 'open' ORDER BY created_at DESC LIMIT 1), ?)
         ON CONFLICT(invoice_id) DO NOTHING
         RETURNING billing_cycle_id",
    )
    .bind(&id)
    .bind(invoice_id)
    .bind(merchant_id)
    .bind(fee_amount_zec)
    .bind(fee_amount_zatoshis)
    .bind(fee_rate_applied)
    .bind(merchant_id)
    .bind(&now)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((Some(cid),)) = inserted {
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
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    tracing::debug!(invoice_id, fee_amount_zec, "Fee entry created");
    Ok(())
}

#[cfg(test)]
pub async fn mark_fee_collected(pool: &SqlitePool, invoice_id: &str) -> anyhow::Result<()> {
    let amount: Option<i64> =
        sqlx::query_scalar("SELECT fee_amount_zatoshis FROM fee_ledger WHERE invoice_id=?")
            .bind(invoice_id)
            .fetch_optional(pool)
            .await?;
    if let Some(amount) = amount {
        apply_collected(pool, invoice_id, amount).await?;
    }
    Ok(())
}

pub async fn record_direct_receipt(
    pool: &SqlitePool,
    invoice: &str,
    txid: &str,
    units: i64,
) -> anyhow::Result<()> {
    anyhow::ensure!(units > 0, "Invalid fee receipt");
    // Caller has verified raw transaction outputs and all contributing ZEC
    // transactions' confirmations. A mempool observation cannot earn rewards.
    sqlx::query("INSERT INTO direct_fee_receipts(invoice_id,txid,units) VALUES (?,?,?) ON CONFLICT(invoice_id,txid) DO NOTHING")
        .bind(invoice).bind(txid).bind(units).execute(pool).await?;
    let total: i64 =
        sqlx::query_scalar("SELECT SUM(units) FROM direct_fee_receipts WHERE invoice_id=?")
            .bind(invoice)
            .fetch_one(pool)
            .await?;
    apply_collected(pool, invoice, total).await?;
    Ok(())
}

async fn apply_collected(pool: &SqlitePool, invoice: &str, received: i64) -> anyhow::Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let old: Option<(i64,i64,Option<String>)>=sqlx::query_as("SELECT fee_amount_zatoshis,collected_zatoshis,billing_cycle_id FROM fee_ledger WHERE invoice_id=?")
        .bind(invoice).fetch_optional(&mut *tx).await?;
    if let Some((due, previous, cycle)) = old {
        let collected = received.min(due).max(previous);
        let delta = collected - previous;
        sqlx::query("UPDATE fee_ledger SET collected_zatoshis=?,auto_collected=?,collected_at=CASE WHEN ?>=fee_amount_zatoshis THEN strftime('%Y-%m-%dT%H:%M:%SZ','now') ELSE collected_at END WHERE invoice_id=?")
            .bind(collected).bind(collected>=due).bind(collected).bind(invoice).execute(&mut *tx).await?;
        if let Some(cycle) = cycle {
            sqlx::query("UPDATE billing_cycles SET auto_collected_zatoshis=auto_collected_zatoshis+?,outstanding_zatoshis=MAX(0,outstanding_zatoshis-?),
                auto_collected_zec=(auto_collected_zatoshis+?)/100000000.0,outstanding_zec=MAX(0,outstanding_zatoshis-?)/100000000.0 WHERE id=?")
                .bind(delta).bind(delta).bind(delta).bind(delta).bind(cycle).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> SqlitePool {
        let pool = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&pool, "fees").await;
        crate::repair_tests::invoice(&pool, "payment", "fees").await;
        sqlx::query(
            "INSERT INTO billing_cycles (id, merchant_id, period_start, period_end, status)
            VALUES ('cycle', 'fees', '2026-10-01', '2026-11-01', 'open')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn totals(pool: &SqlitePool) -> (i64, i64, i64) {
        sqlx::query_as(
            "SELECT total_fees_zatoshis, auto_collected_zatoshis, outstanding_zatoshis
            FROM billing_cycles WHERE id='cycle'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn repeated_and_concurrent_fee_events_charge_once() {
        let pool = fixture().await;
        let (a, b) = tokio::join!(
            create_fee_entry(&pool, "payment", "fees", 0.001, 0.01),
            create_fee_entry(&pool, "payment", "fees", 0.001, 0.01)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(totals(&pool).await, (100_000, 0, 100_000));
        let (a, b) = tokio::join!(
            mark_fee_collected(&pool, "payment"),
            mark_fee_collected(&pool, "payment")
        );
        a.unwrap();
        b.unwrap();
        // A late retry cannot reopen a collected liability, even after a rate change.
        create_fee_entry(&pool, "payment", "fees", 0.002, 0.02)
            .await
            .unwrap();
        assert_eq!(totals(&pool).await, (100_000, 100_000, 0));
    }

    #[tokio::test]
    async fn aggregate_failure_rolls_back_fee_insert_and_collection() {
        let pool = fixture().await;
        sqlx::query(
            "CREATE TRIGGER fail_cycle_update BEFORE UPDATE ON billing_cycles
            BEGIN SELECT RAISE(ABORT, 'test failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(create_fee_entry(&pool, "payment", "fees", 0.001, 0.01)
            .await
            .is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fee_ledger")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER fail_cycle_update")
            .execute(&pool)
            .await
            .unwrap();
        create_fee_entry(&pool, "payment", "fees", 0.001, 0.01)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_cycle_update BEFORE UPDATE ON billing_cycles
            BEGIN SELECT RAISE(ABORT, 'test failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(mark_fee_collected(&pool, "payment").await.is_err());
        let collected: i64 =
            sqlx::query_scalar("SELECT auto_collected FROM fee_ledger WHERE invoice_id='payment'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(collected, 0);
        assert_eq!(totals(&pool).await, (100_000, 0, 100_000));
        sqlx::query("DROP TRIGGER fail_cycle_update")
            .execute(&pool)
            .await
            .unwrap();
        mark_fee_collected(&pool, "payment").await.unwrap();
        assert_eq!(totals(&pool).await, (100_000, 100_000, 0));
    }
}

#[cfg(test)]
mod actual_receipt_tests {
    use super::*;
    #[tokio::test]
    async fn partial_receipts_reduce_only_actual_liability_and_replays_are_free() {
        let p = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&p, "m").await;
        crate::repair_tests::invoice(&p, "i", "m").await;
        sqlx::query("INSERT INTO billing_cycles(id,merchant_id,period_start,period_end,status) VALUES ('c','m','2026-01-01','2026-02-01','open')").execute(&p).await.unwrap();
        create_fee_entry(&p, "i", "m", 0.001, 0.01).await.unwrap();
        for _ in 0..2 {
            record_direct_receipt(&p, "i", "part", 1).await.unwrap();
        }
        let balance: (i64, i64) = sqlx::query_as(
            "SELECT outstanding_zatoshis,auto_collected_zatoshis FROM billing_cycles WHERE id='c'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(balance, (99999, 1));
        let fully: bool =
            sqlx::query_scalar("SELECT auto_collected FROM fee_ledger WHERE invoice_id='i'")
                .fetch_one(&p)
                .await
                .unwrap();
        assert!(!fully);
        record_direct_receipt(&p, "i", "rest", 99999).await.unwrap();
        let balance: (i64, i64) = sqlx::query_as(
            "SELECT outstanding_zatoshis,auto_collected_zatoshis FROM billing_cycles WHERE id='c'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(balance, (0, 100000));
    }
}
