use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;
use chrono::{Utc, Duration as ChronoDuration};

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
}

#[derive(Debug, Deserialize)]
pub struct CreateSubscriptionRequest {
    pub price_id: String,
    pub label: Option<String>,
}

const SUB_COLS: &str = "id, merchant_id, price_id, label, status, current_period_start, current_period_end, cancel_at_period_end, canceled_at, created_at";

fn compute_period_end(start: &chrono::DateTime<Utc>, interval: &str, count: i32) -> chrono::DateTime<Utc> {
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
    let price = crate::prices::get_price(pool, &req.price_id).await?
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
        .format("%Y-%m-%dT%H:%M:%SZ").to_string();

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

    get_subscription(pool, &id).await?
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

pub async fn list_subscriptions(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<Vec<Subscription>> {
    let q = format!("SELECT {} FROM subscriptions WHERE merchant_id = ? ORDER BY created_at DESC", SUB_COLS);
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
        sqlx::query("UPDATE subscriptions SET cancel_at_period_end = 1, canceled_at = ? WHERE id = ?")
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

/// Advance subscriptions past their period end.
/// For active subs past their period: advance the period.
/// For subs with cancel_at_period_end: mark as canceled.
#[allow(dead_code)]
pub async fn process_renewals(pool: &SqlitePool) -> anyhow::Result<u32> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    sqlx::query(
        "UPDATE subscriptions SET status = 'canceled'
         WHERE cancel_at_period_end = 1 AND current_period_end <= ? AND status = 'active'"
    )
    .bind(&now)
    .execute(pool)
    .await?;

    let q = format!(
        "SELECT {} FROM subscriptions WHERE status = 'active' AND current_period_end <= ? AND cancel_at_period_end = 0",
        SUB_COLS
    );
    let due: Vec<Subscription> = sqlx::query_as::<_, Subscription>(&q)
        .bind(&now)
        .fetch_all(pool)
        .await?;

    let mut renewed = 0u32;
    for sub in &due {
        let price = match crate::prices::get_price(pool, &sub.price_id).await? {
            Some(p) if p.active == 1 => p,
            _ => {
                tracing::warn!(sub_id = %sub.id, "Subscription price inactive, marking past_due");
                sqlx::query("UPDATE subscriptions SET status = 'past_due' WHERE id = ?")
                    .bind(&sub.id).execute(pool).await?;
                continue;
            }
        };

        let interval = price.billing_interval.as_deref().unwrap_or("month");
        let count = price.interval_count.unwrap_or(1);
        let now_dt = Utc::now();
        let new_start = now_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let new_end = compute_period_end(&now_dt, interval, count)
            .format("%Y-%m-%dT%H:%M:%SZ").to_string();

        sqlx::query(
            "UPDATE subscriptions SET current_period_start = ?, current_period_end = ? WHERE id = ?"
        )
        .bind(&new_start)
        .bind(&new_end)
        .bind(&sub.id)
        .execute(pool)
        .await?;

        tracing::info!(sub_id = %sub.id, next_end = %new_end, "Subscription period advanced");
        renewed += 1;
    }

    Ok(renewed)
}
