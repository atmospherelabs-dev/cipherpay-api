mod fee_ledger;
mod status;
mod types;

use chrono::{Duration, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;

pub use fee_ledger::{create_fee_entry, get_effective_fee_rate, mark_fee_collected};
pub use status::get_merchant_billing_status;
pub use types::{BillingCycle, BillingSummary, MIN_SETTLEMENT_ZATOSHIS};

pub async fn get_billing_summary(
    pool: &SqlitePool,
    merchant_id: &str,
    config: &Config,
) -> anyhow::Result<BillingSummary> {
    let (trust_tier, billing_status): (String, String) = sqlx::query_as(
        "SELECT COALESCE(trust_tier, 'new'), COALESCE(billing_status, 'active')
         FROM merchants WHERE id = ?",
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;

    let current_cycle: Option<BillingCycle> = sqlx::query_as(
        "SELECT * FROM billing_cycles WHERE merchant_id = ? AND status IN ('open', 'invoiced')
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    let (total_fees, auto_collected, outstanding) = match &current_cycle {
        Some(c) => (c.total_fees_zec, c.auto_collected_zec, c.outstanding_zec),
        None => (0.0, 0.0, 0.0),
    };
    let (total_fees_zatoshis, auto_collected_zatoshis, outstanding_zatoshis) = match &current_cycle
    {
        Some(c) => (
            c.total_fees_zatoshis,
            c.auto_collected_zatoshis,
            c.outstanding_zatoshis,
        ),
        None => (0, 0, 0),
    };

    let settlement_invoice_status: Option<String> = match &current_cycle {
        Some(c) if c.settlement_invoice_id.is_some() => {
            sqlx::query_scalar("SELECT status FROM invoices WHERE id = ?")
                .bind(c.settlement_invoice_id.as_ref().unwrap())
                .fetch_optional(pool)
                .await?
        }
        _ => None,
    };

    let effective_fee_rate = get_effective_fee_rate(pool, merchant_id, config).await?;

    Ok(BillingSummary {
        fee_rate: effective_fee_rate,
        trust_tier,
        billing_status,
        current_cycle,
        total_fees_zec: total_fees,
        auto_collected_zec: auto_collected,
        outstanding_zec: outstanding,
        total_fees_zatoshis,
        auto_collected_zatoshis,
        outstanding_zatoshis,
        settlement_invoice_status,
    })
}

pub async fn get_billing_history(
    pool: &SqlitePool,
    merchant_id: &str,
) -> anyhow::Result<Vec<BillingCycle>> {
    let cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE merchant_id = ?
         ORDER BY period_start DESC LIMIT 24",
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(cycles)
}

pub async fn ensure_billing_cycle(
    pool: &SqlitePool,
    merchant_id: &str,
    config: &Config,
) -> anyhow::Result<()> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM billing_cycles WHERE merchant_id = ? AND status = 'open' LIMIT 1",
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    if existing.is_some() {
        return Ok(());
    }

    let (trust_tier,): (String,) =
        sqlx::query_as("SELECT COALESCE(trust_tier, 'new') FROM merchants WHERE id = ?")
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
    let period_end = (now + Duration::days(cycle_days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let carried: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(outstanding_zatoshis), 0) FROM billing_cycles
         WHERE merchant_id = ? AND status = 'carried_over'",
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let carried_zec = crate::invoices::zatoshis_to_zec(carried);

    sqlx::query(
        "INSERT INTO billing_cycles (
            id, merchant_id, period_start, period_end,
            total_fees_zec, auto_collected_zec, outstanding_zec,
            total_fees_zatoshis, auto_collected_zatoshis, outstanding_zatoshis,
            status
         )
         VALUES (?, ?, ?, ?, ?, 0.0, ?, ?, 0, ?, 'open')",
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&period_start)
    .bind(&period_end)
    .bind(carried_zec)
    .bind(carried_zec)
    .bind(carried)
    .bind(carried)
    .execute(pool)
    .await?;

    if carried > 0 {
        sqlx::query(
            "UPDATE billing_cycles SET outstanding_zec = 0.0, outstanding_zatoshis = 0
             WHERE merchant_id = ? AND status = 'carried_over' AND outstanding_zatoshis > 0",
        )
        .bind(merchant_id)
        .execute(pool)
        .await?;
        tracing::info!(
            merchant_id,
            carried,
            "Carried over outstanding fees to new cycle"
        );
    }

    sqlx::query(
        "UPDATE merchants SET billing_started_at = COALESCE(billing_started_at, ?) WHERE id = ?",
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
    outstanding_zatoshis: i64,
    fee_address: &str,
    zec_eur_rate: f64,
    zec_usd_rate: f64,
) -> anyhow::Result<String> {
    let id = Uuid::new_v4().to_string();
    let memo_code = format!("SETTLE-{}", &Uuid::new_v4().to_string()[..8].to_uppercase());
    let now = Utc::now();
    let expires_at = (now + Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let created_at = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let outstanding_zec = crate::invoices::zatoshis_to_zec(outstanding_zatoshis);
    let price_eur = outstanding_zec * zec_eur_rate;
    let price_usd = outstanding_zec * zec_usd_rate;
    let price_zatoshis = outstanding_zatoshis;

    let memo_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        memo_code.as_bytes(),
    );
    let zcash_uri = format!(
        "zcash:{}?amount={:.8}&memo={}",
        fee_address, outstanding_zec, memo_b64
    );

    sqlx::query(
        "INSERT INTO invoices (id, merchant_id, memo_code, product_name, price_eur, price_usd, currency, price_zec,
         zec_rate_at_creation, payment_address, zcash_uri, status, expires_at, created_at, price_zatoshis)
         VALUES (?, ?, ?, 'Fee Settlement', ?, ?, 'EUR', ?, ?, ?, ?, 'pending', ?, ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&memo_code)
    .bind(price_eur)
    .bind(price_usd)
    .bind(outstanding_zec)
    .bind(zec_eur_rate)
    .bind(fee_address)
    .bind(&zcash_uri)
    .bind(&expires_at)
    .bind(&created_at)
    .bind(price_zatoshis)
    .execute(pool)
    .await?;

    tracing::info!(merchant_id, outstanding_zec, price_eur, invoice_id = %id, "Settlement invoice created");
    Ok(id)
}

/// Runs billing cycle processing: close expired cycles, enforce, upgrade tiers, send notifications.
pub async fn process_billing_cycles(
    pool: &SqlitePool,
    config: &Config,
    zec_eur: f64,
    zec_usd: f64,
) -> anyhow::Result<()> {
    if !config.fee_enabled() {
        return Ok(());
    }

    let now_str = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let enc_key = &config.encryption_key;

    // 1. Close expired open cycles
    let expired_cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE status = 'open' AND period_end < ?",
    )
    .bind(&now_str)
    .fetch_all(pool)
    .await?;

    for cycle in &expired_cycles {
        if cycle.outstanding_zatoshis <= 0 {
            sqlx::query("UPDATE billing_cycles SET status = 'paid' WHERE id = ?")
                .bind(&cycle.id)
                .execute(pool)
                .await?;
            tracing::info!(merchant_id = %cycle.merchant_id, "Billing cycle closed (fully collected)");
        } else if cycle.outstanding_zatoshis < MIN_SETTLEMENT_ZATOSHIS {
            sqlx::query("UPDATE billing_cycles SET status = 'carried_over' WHERE id = ?")
                .bind(&cycle.id)
                .execute(pool)
                .await?;
            tracing::info!(
                merchant_id = %cycle.merchant_id,
                outstanding = cycle.outstanding_zec,
                "Billing cycle closed (below minimum, carrying over to next cycle)"
            );
        } else if let Some(fee_addr) = &config.fee_address {
            let grace_days: i64 = match get_trust_tier(pool, &cycle.merchant_id).await?.as_str() {
                "new" => 7,
                "trusted" => 14,
                _ => 7,
            };
            let grace_until = (Utc::now() + Duration::days(grace_days))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();

            let settlement_id = create_settlement_invoice(
                pool,
                &cycle.merchant_id,
                cycle.outstanding_zatoshis,
                fee_addr,
                zec_eur,
                zec_usd,
            )
            .await?;

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

            if let Some(email) =
                crate::email::get_merchant_email(pool, &cycle.merchant_id, enc_key).await
            {
                let _ = crate::email::send_settlement_invoice_email(
                    pool,
                    config,
                    &email,
                    &cycle.merchant_id,
                    &cycle.id,
                    cycle.outstanding_zec,
                    &grace_until,
                    grace_days,
                )
                .await;
            }
        }

        ensure_billing_cycle(pool, &cycle.merchant_id, config).await?;
    }

    // 1b. Grace period reminders (3 days before grace expires)
    let reminder_threshold = (Utc::now() + Duration::days(3))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let invoiced_cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE status = 'invoiced' AND grace_until <= ? AND grace_until > ?",
    )
    .bind(&reminder_threshold)
    .bind(&now_str)
    .fetch_all(pool)
    .await?;

    for cycle in &invoiced_cycles {
        if let Some(ref grace_until) = cycle.grace_until {
            if let Some(email) =
                crate::email::get_merchant_email(pool, &cycle.merchant_id, enc_key).await
            {
                let days_left = {
                    let grace_dt =
                        chrono::NaiveDateTime::parse_from_str(grace_until, "%Y-%m-%dT%H:%M:%SZ");
                    match grace_dt {
                        Ok(dt) => (dt - Utc::now().naive_utc()).num_days().max(1),
                        Err(_) => 3,
                    }
                };
                let _ = crate::email::send_billing_reminder_email(
                    pool,
                    config,
                    &email,
                    &cycle.merchant_id,
                    &cycle.id,
                    cycle.outstanding_zec,
                    grace_until,
                    days_left,
                )
                .await;
            }
        }
    }

    // 2. Enforce past due
    let overdue_cycles = sqlx::query_as::<_, BillingCycle>(
        "SELECT * FROM billing_cycles WHERE status = 'invoiced' AND grace_until < ?",
    )
    .bind(&now_str)
    .fetch_all(pool)
    .await?;

    for cycle in &overdue_cycles {
        if cycle.outstanding_zatoshis < MIN_SETTLEMENT_ZATOSHIS {
            sqlx::query("UPDATE billing_cycles SET status = 'carried_over' WHERE id = ?")
                .bind(&cycle.id)
                .execute(pool)
                .await?;
            sqlx::query("UPDATE merchants SET billing_status = 'active' WHERE id = ?")
                .bind(&cycle.merchant_id)
                .execute(pool)
                .await?;
            tracing::info!(merchant_id = %cycle.merchant_id, outstanding = cycle.outstanding_zec, "Overdue cycle below minimum — carrying over");
            continue;
        }
        sqlx::query("UPDATE billing_cycles SET status = 'past_due' WHERE id = ?")
            .bind(&cycle.id)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE merchants SET billing_status = 'past_due' WHERE id = ?")
            .bind(&cycle.merchant_id)
            .execute(pool)
            .await?;
        tracing::warn!(merchant_id = %cycle.merchant_id, outstanding = cycle.outstanding_zec, "Merchant billing past due");

        if let Some(email) =
            crate::email::get_merchant_email(pool, &cycle.merchant_id, enc_key).await
        {
            let _ = crate::email::send_past_due_email(
                pool,
                config,
                &email,
                &cycle.merchant_id,
                &cycle.id,
                cycle.outstanding_zec,
            )
            .await;
        }
    }

    // 3. Enforce suspension (7 days after past_due for new, 14 for standard/trusted)
    let past_due_cycles =
        sqlx::query_as::<_, BillingCycle>("SELECT * FROM billing_cycles WHERE status = 'past_due'")
            .fetch_all(pool)
            .await?;

    for cycle in &past_due_cycles {
        let suspend_days: i64 = match get_trust_tier(pool, &cycle.merchant_id).await?.as_str() {
            "new" => 7,
            "trusted" => 30,
            _ => 14,
        };

        if let Some(grace_until) = &cycle.grace_until {
            if let Ok(grace_dt) =
                chrono::NaiveDateTime::parse_from_str(grace_until, "%Y-%m-%dT%H:%M:%SZ")
            {
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

                    if let Some(email) =
                        crate::email::get_merchant_email(pool, &cycle.merchant_id, enc_key).await
                    {
                        let _ = crate::email::send_suspended_email(
                            pool,
                            config,
                            &email,
                            &cycle.merchant_id,
                            &cycle.id,
                            cycle.outstanding_zec,
                        )
                        .await;
                    }
                }
            }
        }
    }

    // 4. Expire referral fee discounts
    let expiring_merchants: Vec<(String, Option<f64>)> = sqlx::query_as(
        "SELECT id, fee_rate FROM merchants WHERE fee_discount_until IS NOT NULL AND fee_discount_until < ?",
    )
    .bind(&now_str)
    .fetch_all(pool)
    .await?;

    for (merchant_id, _old_rate) in &expiring_merchants {
        sqlx::query("UPDATE merchants SET fee_rate = NULL, fee_discount_until = NULL WHERE id = ?")
            .bind(merchant_id)
            .execute(pool)
            .await?;
        tracing::info!(
            merchant_id,
            "Fee discount expired, reverted to standard rate"
        );

        if let Some(email) = crate::email::get_merchant_email(pool, merchant_id, enc_key).await {
            let _ = crate::email::send_discount_expired_email(
                pool,
                config,
                &email,
                merchant_id,
                config.fee_rate,
            )
            .await;
        }
    }

    // 4b. Warn merchants whose discount expires in 7 days
    let warning_threshold = (Utc::now() + Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let warning_merchants: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, fee_discount_until FROM merchants
         WHERE fee_discount_until IS NOT NULL AND fee_discount_until > ? AND fee_discount_until <= ?",
    )
    .bind(&now_str)
    .bind(&warning_threshold)
    .fetch_all(pool)
    .await?;

    for (merchant_id, discount_until) in &warning_merchants {
        if let Some(ref until) = discount_until {
            if let Some(email) = crate::email::get_merchant_email(pool, merchant_id, enc_key).await
            {
                let _ = crate::email::send_discount_expiry_warning_email(
                    pool,
                    config,
                    &email,
                    merchant_id,
                    config.fee_rate,
                    until,
                )
                .await;
            }
        }
    }

    // 5. Upgrade trust tiers: 3+ consecutive paid on time
    let merchants_for_upgrade: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, COALESCE(trust_tier, 'new') FROM merchants WHERE trust_tier != 'trusted'",
    )
    .fetch_all(pool)
    .await?;

    for (merchant_id, current_tier) in &merchants_for_upgrade {
        let paid_count: i32 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM (
                SELECT status
                FROM billing_cycles
                WHERE merchant_id = ?
                ORDER BY period_end DESC
                LIMIT 3
             ) recent_cycles
             WHERE status IN ('paid', 'carried_over')",
        )
        .bind(merchant_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        let late_count: i32 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_cycles
             WHERE merchant_id = ? AND status IN ('past_due', 'suspended')
             AND period_end > datetime('now', '-90 days')",
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
pub async fn check_settlement_payments(pool: &SqlitePool, config: &Config) -> anyhow::Result<()> {
    let settled = sqlx::query_as::<_, BillingCycle>(
        "SELECT bc.* FROM billing_cycles bc
         JOIN invoices i ON i.id = bc.settlement_invoice_id
         WHERE bc.status IN ('invoiced', 'past_due', 'suspended')
         AND i.status = 'confirmed'",
    )
    .fetch_all(pool)
    .await?;

    let enc_key = &config.encryption_key;

    for cycle in &settled {
        sqlx::query(
            "UPDATE billing_cycles
             SET status = 'paid', outstanding_zec = 0.0, outstanding_zatoshis = 0
             WHERE id = ?",
        )
        .bind(&cycle.id)
        .execute(pool)
        .await?;
        sqlx::query("UPDATE merchants SET billing_status = 'active' WHERE id = ?")
            .bind(&cycle.merchant_id)
            .execute(pool)
            .await?;
        tracing::info!(merchant_id = %cycle.merchant_id, "Settlement paid, merchant restored");

        if let Some(email) =
            crate::email::get_merchant_email(pool, &cycle.merchant_id, enc_key).await
        {
            let _ = crate::email::send_payment_confirmed_email(
                pool,
                config,
                &email,
                &cycle.merchant_id,
                &cycle.id,
            )
            .await;
        }
    }

    Ok(())
}

async fn get_trust_tier(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<String> {
    let tier: String =
        sqlx::query_scalar("SELECT COALESCE(trust_tier, 'new') FROM merchants WHERE id = ?")
            .bind(merchant_id)
            .fetch_one(pool)
            .await?;
    Ok(tier)
}
