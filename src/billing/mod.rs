use chrono::{Duration, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct FeeEntry {
    pub id: String,
    pub invoice_id: String,
    pub merchant_id: String,
    pub fee_amount_zec: f64,
    pub auto_collected: i32,
    pub collected_at: Option<String>,
    pub billing_cycle_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BillingCycle {
    pub id: String,
    pub merchant_id: String,
    pub period_start: String,
    pub period_end: String,
    pub total_fees_zec: f64,
    pub auto_collected_zec: f64,
    pub outstanding_zec: f64,
    pub settlement_invoice_id: Option<String>,
    pub status: String,
    pub grace_until: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct BillingSummary {
    pub fee_rate: f64,
    pub trust_tier: String,
    pub billing_status: String,
    pub current_cycle: Option<BillingCycle>,
    pub total_fees_zec: f64,
    pub auto_collected_zec: f64,
    pub outstanding_zec: f64,
}

pub async fn create_fee_entry(
    pool: &SqlitePool,
    invoice_id: &str,
    merchant_id: &str,
    fee_amount_zec: f64,
) -> anyhow::Result<()> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let cycle_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM billing_cycles WHERE merchant_id = ? AND status = 'open' LIMIT 1"
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    sqlx::query(
        "INSERT OR IGNORE INTO fee_ledger (id, invoice_id, merchant_id, fee_amount_zec, billing_cycle_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(invoice_id)
    .bind(merchant_id)
    .bind(fee_amount_zec)
    .bind(&cycle_id)
    .bind(&now)
    .execute(pool)
    .await?;

    if let Some(cid) = &cycle_id {
        sqlx::query(
            "UPDATE billing_cycles SET
                total_fees_zec = total_fees_zec + ?,
                outstanding_zec = outstanding_zec + ?
             WHERE id = ?"
        )
        .bind(fee_amount_zec)
        .bind(fee_amount_zec)
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
         WHERE invoice_id = ? AND auto_collected = 0"
    )
    .bind(&now)
    .bind(invoice_id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        let entry: Option<(f64, Option<String>)> = sqlx::query_as(
            "SELECT fee_amount_zec, billing_cycle_id FROM fee_ledger WHERE invoice_id = ?"
        )
        .bind(invoice_id)
        .fetch_optional(pool)
        .await?;

        if let Some((amount, Some(cycle_id))) = entry {
            sqlx::query(
                "UPDATE billing_cycles SET
                    auto_collected_zec = auto_collected_zec + ?,
                    outstanding_zec = MAX(0, outstanding_zec - ?)
                 WHERE id = ?"
            )
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

pub async fn get_billing_summary(
    pool: &SqlitePool,
    merchant_id: &str,
    config: &Config,
) -> anyhow::Result<BillingSummary> {
    let (trust_tier, billing_status): (String, String) = sqlx::query_as(
        "SELECT COALESCE(trust_tier, 'new'), COALESCE(billing_status, 'active')
         FROM merchants WHERE id = ?"
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;

    let current_cycle: Option<BillingCycle> = sqlx::query_as(
        "SELECT * FROM billing_cycles WHERE merchant_id = ? AND status = 'open'
         ORDER BY created_at DESC LIMIT 1"
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    let (total_fees, auto_collected, outstanding) = match &current_cycle {
        Some(c) => (c.total_fees_zec, c.auto_collected_zec, c.outstanding_zec),
        None => (0.0, 0.0, 0.0),
    };

    Ok(BillingSummary {
        fee_rate: config.fee_rate,
        trust_tier,
        billing_status,
        current_cycle,
        total_fees_zec: total_fees,
        auto_collected_zec: auto_collected,
        outstanding_zec: outstanding,
    })
}

pub async fn get_billing_history(
    pool: &SqlitePool,
    merchant_id: &str,
) -> anyhow::Result<Vec<BillingCycle>> {
    let cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE merchant_id = ?
         ORDER BY period_start DESC LIMIT 24"
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(cycles)
}

pub async fn ensure_billing_cycle(pool: &SqlitePool, merchant_id: &str, config: &Config) -> anyhow::Result<()> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM billing_cycles WHERE merchant_id = ? AND status = 'open' LIMIT 1"
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    if existing.is_some() {
        return Ok(());
    }

    let (trust_tier,): (String,) = sqlx::query_as(
        "SELECT COALESCE(trust_tier, 'new') FROM merchants WHERE id = ?"
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;

    let cycle_days = match trust_tier.as_str() {
        "new" => config.billing_cycle_days_new,
        _ => config.billing_cycle_days_standard,
    };

    let now = Utc::now();
    let id = Uuid::new_v4().to_string();
    let period_start = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let period_end = (now + Duration::days(cycle_days)).format("%Y-%m-%dT%H:%M:%SZ").to_string();

    sqlx::query(
        "INSERT INTO billing_cycles (id, merchant_id, period_start, period_end, status)
         VALUES (?, ?, ?, ?, 'open')"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&period_start)
    .bind(&period_end)
    .execute(pool)
    .await?;

    sqlx::query(
        "UPDATE merchants SET billing_started_at = COALESCE(billing_started_at, ?) WHERE id = ?"
    )
    .bind(&period_start)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id, cycle_days, "Billing cycle created");
    Ok(())
}

pub async fn create_settlement_invoice(
    pool: &SqlitePool,
    merchant_id: &str,
    outstanding_zec: f64,
    fee_address: &str,
) -> anyhow::Result<String> {
    let id = Uuid::new_v4().to_string();
    let memo_code = format!("SETTLE-{}", &Uuid::new_v4().to_string()[..8].to_uppercase());
    let now = Utc::now();
    let expires_at = (now + Duration::days(7)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let created_at = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let memo_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        memo_code.as_bytes(),
    );
    let zcash_uri = format!(
        "zcash:{}?amount={:.8}&memo={}",
        fee_address, outstanding_zec, memo_b64
    );

    sqlx::query(
        "INSERT INTO invoices (id, merchant_id, memo_code, product_name, price_eur, price_zec,
         zec_rate_at_creation, payment_address, zcash_uri, status, expires_at, created_at)
         VALUES (?, ?, ?, 'Fee Settlement', 0.0, ?, 0.0, ?, ?, 'pending', ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&memo_code)
    .bind(outstanding_zec)
    .bind(fee_address)
    .bind(&zcash_uri)
    .bind(&expires_at)
    .bind(&created_at)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id, outstanding_zec, invoice_id = %id, "Settlement invoice created");
    Ok(id)
}

/// Runs billing cycle processing: close expired cycles, enforce, upgrade tiers.
pub async fn process_billing_cycles(pool: &SqlitePool, config: &Config) -> anyhow::Result<()> {
    if !config.fee_enabled() {
        return Ok(());
    }

    let now_str = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // 1. Close expired open cycles
    let expired_cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE status = 'open' AND period_end < ?"
    )
    .bind(&now_str)
    .fetch_all(pool)
    .await?;

    for cycle in &expired_cycles {
        if cycle.outstanding_zec <= 0.0001 {
            sqlx::query("UPDATE billing_cycles SET status = 'paid' WHERE id = ?")
                .bind(&cycle.id)
                .execute(pool)
                .await?;
            tracing::info!(merchant_id = %cycle.merchant_id, "Billing cycle closed (fully collected)");
        } else if let Some(fee_addr) = &config.fee_address {
            let grace_days: i64 = match get_trust_tier(pool, &cycle.merchant_id).await?.as_str() {
                "new" => 3,
                "trusted" => 14,
                _ => 7,
            };
            let grace_until = (Utc::now() + Duration::days(grace_days))
                .format("%Y-%m-%dT%H:%M:%SZ").to_string();

            let settlement_id = create_settlement_invoice(
                pool, &cycle.merchant_id, cycle.outstanding_zec, fee_addr,
            ).await?;

            sqlx::query(
                "UPDATE billing_cycles SET status = 'invoiced', settlement_invoice_id = ?, grace_until = ?
                 WHERE id = ?"
            )
            .bind(&settlement_id)
            .bind(&grace_until)
            .bind(&cycle.id)
            .execute(pool)
            .await?;

            tracing::info!(
                merchant_id = %cycle.merchant_id,
                outstanding = cycle.outstanding_zec,
                grace_until = %grace_until,
                "Settlement invoice generated"
            );
        }

        ensure_billing_cycle(pool, &cycle.merchant_id, config).await?;
    }

    // 2. Enforce past due
    let overdue_cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE status = 'invoiced' AND grace_until < ?"
    )
    .bind(&now_str)
    .fetch_all(pool)
    .await?;

    for cycle in &overdue_cycles {
        sqlx::query("UPDATE billing_cycles SET status = 'past_due' WHERE id = ?")
            .bind(&cycle.id)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE merchants SET billing_status = 'past_due' WHERE id = ?")
            .bind(&cycle.merchant_id)
            .execute(pool)
            .await?;
        tracing::warn!(merchant_id = %cycle.merchant_id, "Merchant billing past due");
    }

    // 3. Enforce suspension (7 days after past_due for new, 14 for standard/trusted)
    let past_due_cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE status = 'past_due'"
    )
    .fetch_all(pool)
    .await?;

    for cycle in &past_due_cycles {
        let suspend_days: i64 = match get_trust_tier(pool, &cycle.merchant_id).await?.as_str() {
            "new" => 7,
            "trusted" => 30,
            _ => 14,
        };

        if let Some(grace_until) = &cycle.grace_until {
            if let Ok(grace_dt) = chrono::NaiveDateTime::parse_from_str(grace_until, "%Y-%m-%dT%H:%M:%SZ") {
                let suspend_at = grace_dt + Duration::days(suspend_days);
                if Utc::now().naive_utc() > suspend_at {
                    sqlx::query("UPDATE billing_cycles SET status = 'suspended' WHERE id = ?")
                        .bind(&cycle.id)
                        .execute(pool)
                        .await?;
                    sqlx::query("UPDATE merchants SET billing_status = 'suspended' WHERE id = ?")
                        .bind(&cycle.merchant_id)
                        .execute(pool)
                        .await?;
                    tracing::warn!(merchant_id = %cycle.merchant_id, "Merchant suspended for non-payment");
                }
            }
        }
    }

    // 4. Upgrade trust tiers: 3+ consecutive paid on time
    let merchants_for_upgrade: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, COALESCE(trust_tier, 'new') FROM merchants WHERE trust_tier != 'trusted'"
    )
    .fetch_all(pool)
    .await?;

    for (merchant_id, current_tier) in &merchants_for_upgrade {
        let paid_count: i32 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_cycles
             WHERE merchant_id = ? AND status = 'paid'
             ORDER BY period_end DESC LIMIT 3"
        )
        .bind(merchant_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        let late_count: i32 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_cycles
             WHERE merchant_id = ? AND status IN ('past_due', 'suspended')
             AND period_end > datetime('now', '-90 days')"
        )
        .bind(merchant_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        if late_count == 0 && paid_count >= 3 {
            let new_tier = match current_tier.as_str() {
                "new" => "standard",
                "standard" => "trusted",
                _ => continue,
            };
            sqlx::query("UPDATE merchants SET trust_tier = ? WHERE id = ?")
                .bind(new_tier)
                .bind(merchant_id)
                .execute(pool)
                .await?;
            tracing::info!(merchant_id, new_tier, "Merchant trust tier upgraded");
        }
    }

    Ok(())
}

/// Check if a settlement invoice was paid and restore merchant access.
pub async fn check_settlement_payments(pool: &SqlitePool) -> anyhow::Result<()> {
    let settled = sqlx::query_as::<_, BillingCycle>(
        "SELECT bc.* FROM billing_cycles bc
         JOIN invoices i ON i.id = bc.settlement_invoice_id
         WHERE bc.status IN ('invoiced', 'past_due', 'suspended')
         AND i.status = 'confirmed'"
    )
    .fetch_all(pool)
    .await?;

    for cycle in &settled {
        sqlx::query("UPDATE billing_cycles SET status = 'paid', outstanding_zec = 0.0 WHERE id = ?")
            .bind(&cycle.id)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE merchants SET billing_status = 'active' WHERE id = ?")
            .bind(&cycle.merchant_id)
            .execute(pool)
            .await?;
        tracing::info!(merchant_id = %cycle.merchant_id, "Settlement paid, merchant restored");
    }

    Ok(())
}

async fn get_trust_tier(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<String> {
    let tier: String = sqlx::query_scalar(
        "SELECT COALESCE(trust_tier, 'new') FROM merchants WHERE id = ?"
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;
    Ok(tier)
}

pub async fn get_merchant_billing_status(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<String> {
    let status: String = sqlx::query_scalar(
        "SELECT COALESCE(billing_status, 'active') FROM merchants WHERE id = ?"
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;
    Ok(status)
}
