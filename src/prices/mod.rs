use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Price {
    pub id: String,
    pub product_id: String,
    pub currency: String,
    pub unit_amount: f64,
    pub label: Option<String>,
    pub max_quantity: Option<i64>,
    #[serde(default = "default_price_type")]
    pub price_type: String,
    pub billing_interval: Option<String>,
    pub interval_count: Option<i32>,
    pub active: i32,
    pub created_at: String,
}

fn default_price_type() -> String { "one_time".to_string() }

const VALID_PRICE_TYPES: &[&str] = &["one_time", "recurring"];
const VALID_INTERVALS: &[&str] = &["day", "week", "month", "year"];

#[derive(Debug, Deserialize)]
pub struct CreatePriceRequest {
    pub product_id: String,
    pub currency: String,
    pub unit_amount: f64,
    pub label: Option<String>,
    pub max_quantity: Option<i64>,
    pub price_type: Option<String>,
    pub billing_interval: Option<String>,
    pub interval_count: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePriceRequest {
    pub unit_amount: Option<f64>,
    pub currency: Option<String>,
    pub label: Option<String>,
    pub max_quantity: Option<i64>,
    pub price_type: Option<String>,
    pub billing_interval: Option<String>,
    pub interval_count: Option<i32>,
}

pub const SUPPORTED_CURRENCIES: &[&str] = &["EUR", "USD", "BRL", "GBP", "CAD", "JPY", "MXN", "ARS", "NGN", "CHF", "INR"];
const MAX_UNIT_AMOUNT: f64 = 1_000_000.0;

fn generate_price_id() -> String {
    format!("cprice_{}", Uuid::new_v4().to_string().replace('-', ""))
}

pub async fn create_price(
    pool: &SqlitePool,
    merchant_id: &str,
    req: &CreatePriceRequest,
) -> anyhow::Result<Price> {
    let currency = req.currency.to_uppercase();
    if !SUPPORTED_CURRENCIES.contains(&currency.as_str()) {
        anyhow::bail!("Unsupported currency: {}. Supported: {}", currency, SUPPORTED_CURRENCIES.join(", "));
    }
    if req.unit_amount <= 0.0 {
        anyhow::bail!("unit_amount must be > 0");
    }
    if req.unit_amount > MAX_UNIT_AMOUNT {
        anyhow::bail!("unit_amount exceeds maximum of {}", MAX_UNIT_AMOUNT);
    }
    if let Some(max_q) = req.max_quantity {
        if max_q <= 0 {
            anyhow::bail!("max_quantity must be > 0");
        }
    }

    let product = crate::products::get_product(pool, &req.product_id).await?;
    match product {
        Some(p) if p.merchant_id == merchant_id => {}
        Some(_) => anyhow::bail!("Product does not belong to this merchant"),
        None => anyhow::bail!("Product not found"),
    }

    let is_event_backed = crate::events::is_product_backed_by_event(pool, &req.product_id).await?;
    let existing = get_price_by_product_currency(pool, &req.product_id, &currency).await?;
    if let Some(p) = existing {
        if p.active == 1 && !is_event_backed {
            anyhow::bail!("An active price for {} already exists on this product. Deactivate it first or update it.", currency);
        }
    }

    let price_type = req.price_type.as_deref().unwrap_or("one_time");
    if !VALID_PRICE_TYPES.contains(&price_type) {
        anyhow::bail!("price_type must be one_time or recurring");
    }
    let (billing_interval, interval_count) = if price_type == "recurring" {
        let interval = req.billing_interval.as_deref()
            .ok_or_else(|| anyhow::anyhow!("billing_interval required for recurring prices"))?;
        if !VALID_INTERVALS.contains(&interval) {
            anyhow::bail!("billing_interval must be day, week, month, or year");
        }
        let count = req.interval_count.unwrap_or(1);
        if count < 1 || count > 365 {
            anyhow::bail!("interval_count must be between 1 and 365");
        }
        (Some(interval.to_string()), Some(count))
    } else {
        (None, None)
    };

    let id = generate_price_id();
    sqlx::query(
        "INSERT INTO prices (id, product_id, currency, unit_amount, label, max_quantity, price_type, billing_interval, interval_count)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(&req.product_id)
    .bind(&currency)
    .bind(req.unit_amount)
    .bind(&req.label)
    .bind(req.max_quantity)
    .bind(price_type)
    .bind(&billing_interval)
    .bind(interval_count)
    .execute(pool)
    .await?;

    tracing::info!(price_id = %id, product_id = %req.product_id, currency = %currency, "Price created");

    get_price(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Price not found after insert"))
}

const PRICE_COLS: &str = "id, product_id, currency, unit_amount, label, max_quantity, price_type, billing_interval, interval_count, active, created_at";

pub async fn list_prices_for_product(pool: &SqlitePool, product_id: &str) -> anyhow::Result<Vec<Price>> {
    let q = format!("SELECT {} FROM prices WHERE product_id = ? ORDER BY currency ASC", PRICE_COLS);
    let rows = sqlx::query_as::<_, Price>(&q)
        .bind(product_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn get_price(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Price>> {
    let q = format!("SELECT {} FROM prices WHERE id = ?", PRICE_COLS);
    let row = sqlx::query_as::<_, Price>(&q)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_price_by_product_currency(
    pool: &SqlitePool,
    product_id: &str,
    currency: &str,
) -> anyhow::Result<Option<Price>> {
    let q = format!("SELECT {} FROM prices WHERE product_id = ? AND currency = ? AND active = 1", PRICE_COLS);
    let row = sqlx::query_as::<_, Price>(&q)
        .bind(product_id)
        .bind(currency)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn update_price(
    pool: &SqlitePool,
    price_id: &str,
    merchant_id: &str,
    req: &UpdatePriceRequest,
) -> anyhow::Result<Option<Price>> {
    let price = match get_price(pool, price_id).await? {
        Some(p) => p,
        None => return Ok(None),
    };

    let product = crate::products::get_product(pool, &price.product_id).await?;
    match product {
        Some(p) if p.merchant_id == merchant_id => {}
        _ => anyhow::bail!("Price not found or does not belong to this merchant"),
    }

    let unit_amount = req.unit_amount.unwrap_or(price.unit_amount);
    if unit_amount <= 0.0 {
        anyhow::bail!("unit_amount must be > 0");
    }
    if unit_amount > MAX_UNIT_AMOUNT {
        anyhow::bail!("unit_amount exceeds maximum of {}", MAX_UNIT_AMOUNT);
    }
    if let Some(max_q) = req.max_quantity {
        if max_q <= 0 {
            anyhow::bail!("max_quantity must be > 0");
        }
    }

    let currency = match &req.currency {
        Some(c) => {
            let c = c.to_uppercase();
            if !SUPPORTED_CURRENCIES.contains(&c.as_str()) {
                anyhow::bail!("Unsupported currency: {}", c);
            }
            if c != price.currency {
                let existing = get_price_by_product_currency(pool, &price.product_id, &c).await?;
                let is_event_backed = crate::events::is_product_backed_by_event(pool, &price.product_id).await?;
                if existing.is_some() && !is_event_backed {
                    anyhow::bail!("An active price for {} already exists on this product. Deactivate it first.", c);
                }
            }
            c
        }
        None => price.currency.clone(),
    };

    let label = req.label.as_ref().or(price.label.as_ref());
    let max_quantity = req.max_quantity.or(price.max_quantity);

    let price_type = req.price_type.as_deref().unwrap_or(&price.price_type);
    if !VALID_PRICE_TYPES.contains(&price_type) {
        anyhow::bail!("price_type must be one_time or recurring");
    }
    let (billing_interval, interval_count) = if price_type == "recurring" {
        let interval = req.billing_interval.as_deref()
            .or(price.billing_interval.as_deref())
            .ok_or_else(|| anyhow::anyhow!("billing_interval required for recurring prices"))?;
        if !VALID_INTERVALS.contains(&interval) {
            anyhow::bail!("billing_interval must be day, week, month, or year");
        }
        let count = req.interval_count.or(price.interval_count).unwrap_or(1);
        if count < 1 || count > 365 {
            anyhow::bail!("interval_count must be between 1 and 365");
        }
        (Some(interval.to_string()), Some(count))
    } else {
        (None, None)
    };

    sqlx::query(
        "UPDATE prices
         SET unit_amount = ?, currency = ?, label = ?, max_quantity = ?, price_type = ?, billing_interval = ?, interval_count = ?
         WHERE id = ?"
    )
    .bind(unit_amount)
    .bind(&currency)
    .bind(label)
    .bind(max_quantity)
    .bind(price_type)
    .bind(&billing_interval)
    .bind(interval_count)
    .bind(price_id)
    .execute(pool)
    .await?;

    tracing::info!(price_id, "Price updated");
    get_price(pool, price_id).await
}

pub async fn deactivate_price(
    pool: &SqlitePool,
    price_id: &str,
    merchant_id: &str,
) -> anyhow::Result<bool> {
    let price = match get_price(pool, price_id).await? {
        Some(p) => p,
        None => return Ok(false),
    };

    if price.active == 0 {
        return Ok(true);
    }

    let product = crate::products::get_product(pool, &price.product_id).await?;
    let product = match product {
        Some(p) if p.merchant_id == merchant_id => p,
        _ => return Ok(false),
    };

    let active_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM prices WHERE product_id = ? AND active = 1"
    )
    .bind(&price.product_id)
    .fetch_one(pool)
    .await?;

    if active_count.0 <= 1 {
        anyhow::bail!("Cannot remove the last active price. A product must have at least one price.");
    }

    // If this price is the product's default, reassign to another active price before deactivating
    let is_default = product.default_price_id.as_deref() == Some(price_id);
    if is_default {
        let other_price: Option<String> = sqlx::query_scalar(
            "SELECT id FROM prices WHERE product_id = ? AND active = 1 AND id != ? ORDER BY created_at ASC LIMIT 1"
        )
        .bind(&price.product_id)
        .bind(price_id)
        .fetch_optional(pool)
        .await?;

        if let Some(new_default_id) = other_price {
            crate::products::set_default_price(pool, &price.product_id, &new_default_id).await?;
            tracing::info!(price_id = %price_id, new_default = %new_default_id, "Reassigned default price before deactivation");
        } else {
            anyhow::bail!("Cannot deactivate the default price when it is the only active price.");
        }
    }

    let result = sqlx::query("UPDATE prices SET active = 0 WHERE id = ?")
        .bind(price_id)
        .execute(pool)
        .await?;

    if result.rows_affected() > 0 {
        tracing::info!(price_id, "Price deactivated");
        Ok(true)
    } else {
        Ok(false)
    }
}
