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
}

#[derive(Debug, Deserialize)]
pub struct CreateSubscriptionRequest {
    pub price_id: String,
    pub label: Option<String>,
}

const SUB_COLS: &str = "id, merchant_id, price_id, label, status, current_period_start, current_period_end, cancel_at_period_end, canceled_at, created_at, current_invoice_id";

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
        "SELECT {} FROM subscriptions WHERE status = 'active' AND current_period_end <= ? AND cancel_at_period_end = 0 LIMIT 500",
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
        "SELECT {} FROM subscriptions WHERE status = 'active' AND current_period_end <= ? AND cancel_at_period_end = 0 LIMIT 500",
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
