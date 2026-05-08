use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Subscription {
    pub id: String,
    pub merchant_id: String,
    pub price_id: String,
    pub label: Option<String>,
    pub status: String,
    pub current_period_start: String,
    pub current_period_end: String,
    pub cancel_at_period_end: i32,
    pub canceled_at: Option<String>,
    pub created_at: String,
    pub current_invoice_id: Option<String>,
    pub pause_at_period_end: i32,
}

#[derive(Debug, Deserialize)]
pub struct CreateSubscriptionRequest {
    pub price_id: String,
    pub label: Option<String>,
}

const SUB_COLS: &str = "id, merchant_id, price_id, label, status, current_period_start, current_period_end, cancel_at_period_end, canceled_at, created_at, current_invoice_id, pause_at_period_end";

fn compute_period_end(
    start: &chrono::DateTime<Utc>,
    interval: &str,
    count: i32,
) -> chrono::DateTime<Utc> {
    match interval {
        "day" => *start + ChronoDuration::days(count as i64),
        "week" => *start + ChronoDuration::weeks(count as i64),
        "month" => *start + ChronoDuration::days(30 * count as i64),
        "year" => *start + ChronoDuration::days(365 * count as i64),
        _ => *start + ChronoDuration::days(30),
    }
}

pub async fn create_subscription(
    pool: &SqlitePool,
    merchant_id: &str,
    req: &CreateSubscriptionRequest,
) -> anyhow::Result<Subscription> {
    let price = crate::prices::get_price(pool, &req.price_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Price not found"))?;

    if price.price_type != "recurring" {
        anyhow::bail!("Cannot create subscription for a one-time price");
    }
    if price.active != 1 {
        anyhow::bail!("Price is not active");
    }

    let product = crate::products::get_product(pool, &price.product_id).await?;
    match product {
        Some(p) if p.merchant_id == merchant_id => {}
        Some(_) => anyhow::bail!("Price does not belong to this merchant"),
        None => anyhow::bail!("Product not found"),
    }

    let interval = price.billing_interval.as_deref().unwrap_or("month");
    let count = price.interval_count.unwrap_or(1);

    let now = Utc::now();
    let period_start = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let period_end = compute_period_end(&now, interval, count)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let id = format!("sub_{}", Uuid::new_v4().to_string().replace('-', ""));

    sqlx::query(
        "INSERT INTO subscriptions (id, merchant_id, price_id, label, status, current_period_start, current_period_end)
         VALUES (?, ?, ?, ?, 'active', ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&req.price_id)
    .bind(&req.label)
    .bind(&period_start)
    .bind(&period_end)
    .execute(pool)
    .await?;

    tracing::info!(sub_id = %id, price_id = %req.price_id, "Subscription created");

    get_subscription(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Subscription not found after insert"))
}

pub async fn get_subscription(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Subscription>> {
    let q = format!("SELECT {} FROM subscriptions WHERE id = ?", SUB_COLS);
    let row = sqlx::query_as::<_, Subscription>(&q)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn list_subscriptions(
    pool: &SqlitePool,
    merchant_id: &str,
) -> anyhow::Result<Vec<Subscription>> {
    let q = format!(
        "SELECT {} FROM subscriptions WHERE merchant_id = ? ORDER BY created_at DESC",
        SUB_COLS
    );
    let rows = sqlx::query_as::<_, Subscription>(&q)
        .bind(merchant_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn cancel_subscription(
    pool: &SqlitePool,
    sub_id: &str,
    merchant_id: &str,
    at_period_end: bool,
) -> anyhow::Result<Option<Subscription>> {
    let sub = match get_subscription(pool, sub_id).await? {
        Some(s) if s.merchant_id == merchant_id => s,
        Some(_) => anyhow::bail!("Subscription does not belong to this merchant"),
        None => return Ok(None),
    };

    if sub.status == "canceled" {
        return Ok(Some(sub));
    }

    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    if at_period_end {
        sqlx::query(
            "UPDATE subscriptions SET cancel_at_period_end = 1, canceled_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(sub_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query("UPDATE subscriptions SET status = 'canceled', canceled_at = ? WHERE id = ?")
            .bind(&now)
            .bind(sub_id)
            .execute(pool)
            .await?;
    }

    tracing::info!(sub_id, at_period_end, "Subscription canceled");
    get_subscription(pool, sub_id).await
}

/// Void/detach a pending renewal invoice linked to a subscription.
/// Returns Err if the invoice is already confirmed (money paid — merchant must handle manually).
async fn clear_pending_renewal_invoice(
    pool: &SqlitePool,
    sub: &Subscription,
) -> anyhow::Result<()> {
    let inv_id = match sub.current_invoice_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(()),
    };

    let status: Option<(String,)> = sqlx::query_as("SELECT status FROM invoices WHERE id = ?")
        .bind(inv_id)
        .fetch_optional(pool)
        .await?;

    match status {
        Some((ref s,)) if s == "confirmed" => {
            anyhow::bail!(
                "Renewal invoice {} is already confirmed — cannot pause while payment is settled",
                inv_id
            );
        }
        Some((ref s,)) if s == "draft" || s == "pending" || s == "underpaid" || s == "detected" => {
            sqlx::query("UPDATE invoices SET status = 'expired' WHERE id = ?")
                .bind(inv_id)
                .execute(pool)
                .await?;
            sqlx::query("UPDATE subscriptions SET current_invoice_id = NULL WHERE id = ?")
                .bind(&sub.id)
                .execute(pool)
                .await?;
            tracing::info!(sub_id = %sub.id, invoice_id = inv_id, "Voided pending renewal invoice for pause");
        }
        _ => {
            // expired, refunded, or missing — just detach
            sqlx::query("UPDATE subscriptions SET current_invoice_id = NULL WHERE id = ?")
                .bind(&sub.id)
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}

pub async fn pause_subscription(
    pool: &SqlitePool,
    sub_id: &str,
    merchant_id: &str,
) -> anyhow::Result<Option<Subscription>> {
    let sub = match get_subscription(pool, sub_id).await? {
        Some(s) if s.merchant_id == merchant_id => s,
        Some(_) => anyhow::bail!("Subscription does not belong to this merchant"),
        None => return Ok(None),
    };

    if sub.status != "active" {
        anyhow::bail!("Only active subscriptions can be paused (current: {})", sub.status);
    }
    if sub.cancel_at_period_end != 0 {
        anyhow::bail!("Cannot pause a subscription that is scheduled for cancellation");
    }
    if sub.pause_at_period_end != 0 {
        anyhow::bail!("Subscription is already scheduled to pause");
    }

    clear_pending_renewal_invoice(pool, &sub).await?;

    sqlx::query("UPDATE subscriptions SET pause_at_period_end = 1 WHERE id = ?")
        .bind(sub_id)
        .execute(pool)
        .await?;

    tracing::info!(sub_id, "Subscription scheduled to pause at period end");
    get_subscription(pool, sub_id).await
}

/// Resume a paused subscription or cancel a pending pause.
/// If resuming from `paused`, starts a fresh billing period from now.
/// If canceling a pending pause (still `active`), just clears the flag.
/// Returns `was_paused = true` if the subscription transitioned from paused to active.
pub async fn resume_subscription(
    pool: &SqlitePool,
    sub_id: &str,
    merchant_id: &str,
) -> anyhow::Result<(Option<Subscription>, bool)> {
    let sub = match get_subscription(pool, sub_id).await? {
        Some(s) if s.merchant_id == merchant_id => s,
        Some(_) => anyhow::bail!("Subscription does not belong to this merchant"),
        None => return Ok((None, false)),
    };

    if sub.status == "paused" {
        // Verify the linked price is still active
        let price = crate::prices::get_price(pool, &sub.price_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Linked price no longer exists — cannot resume"))?;
        if price.active != 1 {
            anyhow::bail!("Linked price is no longer active — cannot resume");
        }

        let interval = price.billing_interval.as_deref().unwrap_or("month");
        let count = price.interval_count.unwrap_or(1);
        let now = Utc::now();
        let new_start = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let new_end = compute_period_end(&now, interval, count)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

        sqlx::query(
            "UPDATE subscriptions SET status = 'active', pause_at_period_end = 0,
             current_period_start = ?, current_period_end = ?, current_invoice_id = NULL
             WHERE id = ?",
        )
        .bind(&new_start)
        .bind(&new_end)
        .bind(sub_id)
        .execute(pool)
        .await?;

        tracing::info!(sub_id, new_end = %new_end, "Subscription resumed with fresh period");
        let updated = get_subscription(pool, sub_id).await?;
        Ok((updated, true))
    } else if sub.status == "active" && sub.pause_at_period_end != 0 {
        // Cancel the pending pause — subscription never left active
        sqlx::query("UPDATE subscriptions SET pause_at_period_end = 0 WHERE id = ?")
            .bind(sub_id)
            .execute(pool)
            .await?;

        tracing::info!(sub_id, "Pending pause canceled");
        let updated = get_subscription(pool, sub_id).await?;
        Ok((updated, false))
    } else {
        anyhow::bail!(
            "Subscription cannot be resumed (status: {}, pause_at_period_end: {})",
            sub.status,
            sub.pause_at_period_end
        );
    }
}

const RENEWAL_NOTICE_DAYS: i64 = 3;

/// Full subscription lifecycle engine. Runs hourly via background task.
///
/// 1. Cancel subscriptions marked cancel_at_period_end that are past their period
/// 2. Generate draft invoices for subscriptions due within RENEWAL_NOTICE_DAYS
/// 3. Advance periods for subscriptions with confirmed invoices past period end
/// 4. Mark subscriptions past_due if period ended without payment
pub async fn process_renewals(
    pool: &SqlitePool,
    http: &reqwest::Client,
    encryption_key: &str,
    merchant_ufvks: &std::collections::HashMap<String, String>,
    fee_config: Option<&crate::invoices::FeeConfig>,
) -> anyhow::Result<u32> {
    let now_str = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut actions = 0u32;

    // 0. Pause subscriptions scheduled to pause at period end
    let q = format!(
        "SELECT {} FROM subscriptions WHERE pause_at_period_end = 1 AND current_period_end <= ? AND status = 'active' LIMIT 500",
        SUB_COLS
    );
    let to_pause: Vec<Subscription> = sqlx::query_as::<_, Subscription>(&q)
        .bind(&now_str)
        .fetch_all(pool)
        .await?;

    if !to_pause.is_empty() {
        sqlx::query(
            "UPDATE subscriptions SET status = 'paused', pause_at_period_end = 0
             WHERE pause_at_period_end = 1 AND current_period_end <= ? AND status = 'active'",
        )
        .bind(&now_str)
        .execute(pool)
        .await?;

        tracing::info!(count = to_pause.len(), "Subscriptions paused at period end");

        for sub in &to_pause {
            let payload = serde_json::json!({
                "subscription_id": sub.id,
                "price_id": sub.price_id,
            });
            let _ = crate::webhooks::dispatch_event(
                pool,
                http,
                &sub.merchant_id,
                "subscription.paused",
                payload,
                encryption_key,
            )
            .await;
        }
        actions += to_pause.len() as u32;
    }

    // 1. Cancel subscriptions marked for end-of-period cancellation
    // First, select the ones we're about to cancel (before updating) so we dispatch webhooks only for these
    let q = format!(
        "SELECT {} FROM subscriptions WHERE cancel_at_period_end = 1 AND current_period_end <= ? AND status = 'active' LIMIT 500",
        SUB_COLS
    );
    let to_cancel: Vec<Subscription> = sqlx::query_as::<_, Subscription>(&q)
        .bind(&now_str)
        .fetch_all(pool)
        .await?;

    if !to_cancel.is_empty() {
        // Now update them
        sqlx::query(
            "UPDATE subscriptions SET status = 'canceled'
             WHERE cancel_at_period_end = 1 AND current_period_end <= ? AND status = 'active'",
        )
        .bind(&now_str)
        .execute(pool)
        .await?;

        tracing::info!(
            count = to_cancel.len(),
            "Subscriptions canceled at period end"
        );

        // Fire webhooks only for the subscriptions we just canceled
        for sub in &to_cancel {
            let payload = serde_json::json!({
                "subscription_id": sub.id,
                "price_id": sub.price_id,
            });
            let _ = crate::webhooks::dispatch_event(
                pool,
                http,
                &sub.merchant_id,
                "subscription.canceled",
                payload,
                encryption_key,
            )
            .await;
        }
        actions += to_cancel.len() as u32;
    }

    // 2. Generate draft invoices for active subscriptions approaching period end
    let notice_threshold = (Utc::now() + ChronoDuration::days(RENEWAL_NOTICE_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let q = format!(
        "SELECT {} FROM subscriptions WHERE status = 'active' AND current_period_end <= ? AND cancel_at_period_end = 0 AND pause_at_period_end = 0 LIMIT 500",
        SUB_COLS
    );
    let due_subs: Vec<Subscription> = sqlx::query_as::<_, Subscription>(&q)
        .bind(&notice_threshold)
        .fetch_all(pool)
        .await?;

    for sub in &due_subs {
        // Check if a draft/pending invoice already exists for this period
        if let Some(ref inv_id) = sub.current_invoice_id {
            if !inv_id.is_empty() {
                let existing: Option<(String,)> =
                    sqlx::query_as("SELECT status FROM invoices WHERE id = ?")
                        .bind(inv_id)
                        .fetch_optional(pool)
                        .await?;
                match existing {
                    Some((ref status,))
                        if status == "draft"
                            || status == "pending"
                            || status == "underpaid"
                            || status == "detected" =>
                    {
                        continue; // invoice already in progress
                    }
                    Some((ref status,)) if status == "confirmed" => {
                        continue; // already paid, will be advanced below
                    }
                    _ => {} // expired/refunded/missing — generate new draft
                }
            }
        }

        let price = match crate::prices::get_price(pool, &sub.price_id).await? {
            Some(p) if p.active == 1 => p,
            _ => {
                tracing::warn!(sub_id = %sub.id, "Subscription price inactive, marking past_due");
                sqlx::query("UPDATE subscriptions SET status = 'past_due' WHERE id = ?")
                    .bind(&sub.id)
                    .execute(pool)
                    .await?;
                actions += 1;
                continue;
            }
        };

        let merchant_ufvk = match merchant_ufvks.get(&sub.merchant_id) {
            Some(u) => u,
            None => {
                tracing::warn!(sub_id = %sub.id, merchant_id = %sub.merchant_id, "Merchant UFVK not found for subscription");
                continue;
            }
        };

        let product = crate::products::get_product(pool, &price.product_id).await?;
        let product_name = product.as_ref().map(|p| p.name.as_str());

        match crate::invoices::create_draft_invoice(
            pool,
            &sub.merchant_id,
            merchant_ufvk,
            &sub.id,
            product_name,
            price.unit_amount,
            &price.currency,
            Some(&price.id),
            &sub.current_period_end,
            fee_config,
        )
        .await
        {
            Ok(invoice) => {
                sqlx::query("UPDATE subscriptions SET current_invoice_id = ? WHERE id = ?")
                    .bind(&invoice.id)
                    .bind(&sub.id)
                    .execute(pool)
                    .await?;

                let payload = serde_json::json!({
                    "invoice_id": invoice.id,
                    "subscription_id": sub.id,
                    "amount": price.unit_amount,
                    "currency": price.currency,
                    "due_date": sub.current_period_end,
                });
                let _ = crate::webhooks::dispatch_event(
                    pool,
                    http,
                    &sub.merchant_id,
                    "invoice.created",
                    payload,
                    encryption_key,
                )
                .await;

                tracing::info!(sub_id = %sub.id, invoice_id = %invoice.id, "Draft invoice generated for subscription");
                actions += 1;
            }
            Err(e) => {
                tracing::error!(sub_id = %sub.id, error = %e, "Failed to create draft invoice");
            }
        }
    }

    // 3. Advance paid periods (subscriptions past period_end with confirmed invoice)
    // Note: subscription.renewed webhook is dispatched by the scanner on payment confirmation.
    // This step is a fallback for edge cases (e.g., server restart during scan).
    let q = format!(
        "SELECT {} FROM subscriptions WHERE status = 'active' AND current_period_end <= ? AND cancel_at_period_end = 0 AND pause_at_period_end = 0 LIMIT 500",
        SUB_COLS
    );
    let past_due_candidates: Vec<Subscription> = sqlx::query_as::<_, Subscription>(&q)
        .bind(&now_str)
        .fetch_all(pool)
        .await?;

    for sub in &past_due_candidates {
        if let Some(ref inv_id) = sub.current_invoice_id {
            if !inv_id.is_empty() {
                let inv_status: Option<(String,)> =
                    sqlx::query_as("SELECT status FROM invoices WHERE id = ?")
                        .bind(inv_id)
                        .fetch_optional(pool)
                        .await?;

                if let Some((ref status,)) = inv_status {
                    if status == "confirmed" {
                        // Just advance the period — webhook already dispatched by scanner
                        if advance_subscription_period(pool, &sub.id).await?.is_some() {
                            actions += 1;
                        }
                        continue;
                    }
                }
            }
        }

        // 4. Period ended without confirmed payment → past_due
        tracing::info!(sub_id = %sub.id, "Subscription past due (period ended without payment)");
        sqlx::query(
            "UPDATE subscriptions SET status = 'past_due' WHERE id = ? AND status = 'active'",
        )
        .bind(&sub.id)
        .execute(pool)
        .await?;

        // Expire the draft invoice if it exists
        if let Some(ref inv_id) = sub.current_invoice_id {
            if !inv_id.is_empty() {
                sqlx::query(
                    "UPDATE invoices SET status = 'expired' WHERE id = ? AND status = 'draft'",
                )
                .bind(inv_id)
                .execute(pool)
                .await?;
            }
        }

        let payload = serde_json::json!({
            "subscription_id": sub.id,
            "price_id": sub.price_id,
        });
        let _ = crate::webhooks::dispatch_event(
            pool,
            http,
            &sub.merchant_id,
            "subscription.past_due",
            payload,
            encryption_key,
        )
        .await;
        actions += 1;
    }

    if actions > 0 {
        tracing::info!(actions, "Subscription renewal cycle complete");
    }
    Ok(actions)
}

/// Advance a subscription to its next billing period. Called when invoice is confirmed.
pub async fn advance_subscription_period(
    pool: &SqlitePool,
    sub_id: &str,
) -> anyhow::Result<Option<Subscription>> {
    let sub = match get_subscription(pool, sub_id).await? {
        Some(s) => s,
        None => return Ok(None),
    };

    if sub.status != "active" {
        return Ok(Some(sub));
    }

    let price = match crate::prices::get_price(pool, &sub.price_id).await? {
        Some(p) => p,
        None => return Ok(Some(sub)),
    };

    let interval = price.billing_interval.as_deref().unwrap_or("month");
    let count = price.interval_count.unwrap_or(1);
    let now_dt = Utc::now();
    let new_start = now_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let new_end = compute_period_end(&now_dt, interval, count)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    sqlx::query(
        "UPDATE subscriptions SET current_period_start = ?, current_period_end = ?, current_invoice_id = NULL
         WHERE id = ?"
    )
    .bind(&new_start)
    .bind(&new_end)
    .bind(sub_id)
    .execute(pool)
    .await?;

    tracing::info!(sub_id, new_end = %new_end, "Subscription period advanced");
    get_subscription(pool, sub_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_pool() -> SqlitePool {
        let pool = crate::db::create_pool("sqlite::memory:")
            .await
            .expect("Failed to create test pool");
        pool
    }

    async fn seed_merchant(pool: &SqlitePool, merchant_id: &str) {
        sqlx::query(
            "INSERT INTO merchants (id, name, api_key_hash, dashboard_token_hash, ufvk, payment_address, diversifier_index)
             VALUES (?, 'Test', 'hash', 'dhash', 'ufvk_test', 'zaddr_test', 1)",
        )
        .bind(merchant_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_product_and_price(
        pool: &SqlitePool,
        merchant_id: &str,
        product_id: &str,
        price_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO products (id, merchant_id, slug, name, active) VALUES (?, ?, 'test', 'Test Product', 1)",
        )
        .bind(product_id)
        .bind(merchant_id)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO prices (id, product_id, currency, unit_amount, price_type, billing_interval, interval_count, active)
             VALUES (?, ?, 'USD', 9.99, 'recurring', 'month', 1, 1)",
        )
        .bind(price_id)
        .bind(product_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn create_test_sub(pool: &SqlitePool, merchant_id: &str, price_id: &str) -> Subscription {
        let req = CreateSubscriptionRequest {
            price_id: price_id.to_string(),
            label: None,
        };
        create_subscription(pool, merchant_id, &req).await.unwrap()
    }

    #[tokio::test]
    async fn pause_active_sub_sets_flag() {
        let pool = setup_pool().await;
        let m = "m1";
        let prod = "prod1";
        let price = "price1";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, prod, price).await;
        let sub = create_test_sub(&pool, m, price).await;

        let paused = pause_subscription(&pool, &sub.id, m).await.unwrap().unwrap();
        assert_eq!(paused.status, "active");
        assert_eq!(paused.pause_at_period_end, 1);
    }

    #[tokio::test]
    async fn pause_rejects_canceled_sub() {
        let pool = setup_pool().await;
        let m = "m2";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod2", "price2").await;
        let sub = create_test_sub(&pool, m, "price2").await;

        cancel_subscription(&pool, &sub.id, m, false).await.unwrap();
        let err = pause_subscription(&pool, &sub.id, m).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("Only active"));
    }

    #[tokio::test]
    async fn pause_rejects_already_paused_sub() {
        let pool = setup_pool().await;
        let m = "m3";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod3", "price3").await;
        let sub = create_test_sub(&pool, m, "price3").await;

        pause_subscription(&pool, &sub.id, m).await.unwrap();
        let err = pause_subscription(&pool, &sub.id, m).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("already scheduled"));
    }

    #[tokio::test]
    async fn pause_rejects_cancel_at_period_end() {
        let pool = setup_pool().await;
        let m = "m4";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod4", "price4").await;
        let sub = create_test_sub(&pool, m, "price4").await;

        cancel_subscription(&pool, &sub.id, m, true).await.unwrap();
        let err = pause_subscription(&pool, &sub.id, m).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("scheduled for cancellation"));
    }

    #[tokio::test]
    async fn pause_voids_pending_renewal_invoice() {
        let pool = setup_pool().await;
        let m = "m5";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod5", "price5").await;
        let sub = create_test_sub(&pool, m, "price5").await;

        let inv_id = "inv_test_pending";
        sqlx::query(
            "INSERT INTO invoices (id, merchant_id, memo_code, price_eur, price_zec, zec_rate_at_creation, status, expires_at, subscription_id)
             VALUES (?, ?, 'memo1', 9.99, 0.1, 100.0, 'pending', '2099-01-01T00:00:00Z', ?)",
        )
        .bind(inv_id)
        .bind(m)
        .bind(&sub.id)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("UPDATE subscriptions SET current_invoice_id = ? WHERE id = ?")
            .bind(inv_id)
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();

        let paused = pause_subscription(&pool, &sub.id, m).await.unwrap().unwrap();
        assert_eq!(paused.pause_at_period_end, 1);
        assert!(paused.current_invoice_id.is_none());

        // Verify the invoice was expired
        let inv_status: (String,) = sqlx::query_as("SELECT status FROM invoices WHERE id = ?")
            .bind(inv_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(inv_status.0, "expired");
    }

    #[tokio::test]
    async fn pause_rejects_when_renewal_confirmed() {
        let pool = setup_pool().await;
        let m = "m6";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod6", "price6").await;
        let sub = create_test_sub(&pool, m, "price6").await;

        let inv_id = "inv_test_confirmed";
        sqlx::query(
            "INSERT INTO invoices (id, merchant_id, memo_code, price_eur, price_zec, zec_rate_at_creation, status, expires_at, subscription_id)
             VALUES (?, ?, 'memo2', 9.99, 0.1, 100.0, 'confirmed', '2099-01-01T00:00:00Z', ?)",
        )
        .bind(inv_id)
        .bind(m)
        .bind(&sub.id)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("UPDATE subscriptions SET current_invoice_id = ? WHERE id = ?")
            .bind(inv_id)
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();

        let err = pause_subscription(&pool, &sub.id, m).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("already confirmed"));
    }

    #[tokio::test]
    async fn resume_from_paused_starts_fresh_period() {
        let pool = setup_pool().await;
        let m = "m7";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod7", "price7").await;
        let sub = create_test_sub(&pool, m, "price7").await;

        // Set a stale period and status to paused (simulating process_renewals)
        let stale = "2020-01-01T00:00:00Z";
        sqlx::query(
            "UPDATE subscriptions SET status = 'paused', pause_at_period_end = 0,
             current_period_start = ?, current_period_end = ? WHERE id = ?",
        )
        .bind(stale)
        .bind(stale)
        .bind(&sub.id)
        .execute(&pool)
        .await
        .unwrap();

        let (resumed, was_paused) = resume_subscription(&pool, &sub.id, m).await.unwrap();
        let resumed = resumed.unwrap();
        assert!(was_paused);
        assert_eq!(resumed.status, "active");
        assert_eq!(resumed.pause_at_period_end, 0);
        assert!(resumed.current_invoice_id.is_none());
        assert_ne!(resumed.current_period_start, stale);
    }

    #[tokio::test]
    async fn resume_cancels_pending_pause() {
        let pool = setup_pool().await;
        let m = "m8";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod8", "price8").await;
        let sub = create_test_sub(&pool, m, "price8").await;

        pause_subscription(&pool, &sub.id, m).await.unwrap();

        let (resumed, was_paused) = resume_subscription(&pool, &sub.id, m).await.unwrap();
        let resumed = resumed.unwrap();
        assert!(!was_paused); // Never actually paused
        assert_eq!(resumed.status, "active");
        assert_eq!(resumed.pause_at_period_end, 0);
        // Period should NOT change (pause never took effect)
        assert_eq!(resumed.current_period_start, sub.current_period_start);
    }

    #[tokio::test]
    async fn resume_rejects_past_due() {
        let pool = setup_pool().await;
        let m = "m9";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod9", "price9").await;
        let sub = create_test_sub(&pool, m, "price9").await;

        sqlx::query("UPDATE subscriptions SET status = 'past_due' WHERE id = ?")
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();

        let err = resume_subscription(&pool, &sub.id, m).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("cannot be resumed"));
    }

    #[tokio::test]
    async fn resume_rejects_inactive_price() {
        let pool = setup_pool().await;
        let m = "m10";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod10", "price10").await;
        let sub = create_test_sub(&pool, m, "price10").await;

        sqlx::query("UPDATE subscriptions SET status = 'paused', pause_at_period_end = 0 WHERE id = ?")
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();

        // Deactivate the price
        sqlx::query("UPDATE prices SET active = 0 WHERE id = 'price10'")
            .execute(&pool)
            .await
            .unwrap();

        let err = resume_subscription(&pool, &sub.id, m).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("no longer active"));
    }

    #[tokio::test]
    async fn process_renewals_pauses_at_period_end() {
        let pool = setup_pool().await;
        let m = "m11";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod11", "price11").await;
        let sub = create_test_sub(&pool, m, "price11").await;

        // Schedule pause and fast-forward period end to the past
        pause_subscription(&pool, &sub.id, m).await.unwrap();
        let past = (Utc::now() - ChronoDuration::hours(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        sqlx::query("UPDATE subscriptions SET current_period_end = ? WHERE id = ?")
            .bind(&past)
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();

        let http = reqwest::Client::new();
        let ufvks = std::collections::HashMap::new();
        let actions = process_renewals(&pool, &http, "test_key", &ufvks, None)
            .await
            .unwrap();
        assert!(actions > 0);

        let updated = get_subscription(&pool, &sub.id).await.unwrap().unwrap();
        assert_eq!(updated.status, "paused");
        assert_eq!(updated.pause_at_period_end, 0);
    }

    #[tokio::test]
    async fn paused_subs_skip_renewal_invoice() {
        let pool = setup_pool().await;
        let m = "m12";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod12", "price12").await;
        let sub = create_test_sub(&pool, m, "price12").await;

        // Set to paused
        sqlx::query("UPDATE subscriptions SET status = 'paused', pause_at_period_end = 0 WHERE id = ?")
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();

        let http = reqwest::Client::new();
        let ufvks = std::collections::HashMap::new();
        let actions = process_renewals(&pool, &http, "test_key", &ufvks, None)
            .await
            .unwrap();
        assert_eq!(actions, 0);

        // Verify no invoice was created
        let updated = get_subscription(&pool, &sub.id).await.unwrap().unwrap();
        assert!(updated.current_invoice_id.is_none());
    }

    #[tokio::test]
    async fn advance_skips_non_active_sub() {
        let pool = setup_pool().await;
        let m = "m13";
        seed_merchant(&pool, m).await;
        seed_product_and_price(&pool, m, "prod13", "price13").await;
        let sub = create_test_sub(&pool, m, "price13").await;

        sqlx::query("UPDATE subscriptions SET status = 'paused' WHERE id = ?")
            .bind(&sub.id)
            .execute(&pool)
            .await
            .unwrap();

        let result = advance_subscription_period(&pool, &sub.id).await.unwrap();
        let result = result.unwrap();
        // Should not advance — still paused with original period
        assert_eq!(result.status, "paused");
        assert_eq!(result.current_period_start, sub.current_period_start);
    }
}
