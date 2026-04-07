use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Event {
    pub id: String,
    pub merchant_id: String,
    pub product_id: String,
    pub title: String,
    pub description: Option<String>,
    pub event_date: Option<String>,
    pub event_location: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventContext {
    pub event_title: String,
    pub event_date: Option<String>,
    pub event_location: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateEventPrice {
    pub currency: String,
    pub unit_amount: f64,
    pub label: Option<String>,
    pub max_quantity: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateEventRequest {
    pub title: String,
    pub description: Option<String>,
    pub event_date: Option<String>,
    pub event_location: Option<String>,
    pub prices: Vec<CreateEventPrice>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct EventSummary {
    pub id: String,
    pub product_id: String,
    pub title: String,
    pub description: Option<String>,
    pub event_date: Option<String>,
    pub event_location: Option<String>,
    pub status: String,
    pub created_at: String,
    pub sold_count: i64,
    pub used_count: i64,
    pub total_capacity: Option<i64>,
    pub luma_event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateEventRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub event_date: Option<String>,
    pub event_location: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct EventTierStat {
    pub price_id: String,
    pub label: Option<String>,
    pub currency: String,
    pub unit_amount: f64,
    pub max_quantity: Option<i64>,
    pub sold_count: i64,
    pub used_count: i64,
}

#[derive(Debug, Serialize)]
pub struct EventDetailResponse {
    #[serde(flatten)]
    pub summary: EventSummary,
    pub tiers: Vec<EventTierStat>,
}

/// Parse event_date strings in either "YYYY-MM-DDTHH:MM:SS" or "YYYY-MM-DDTHH:MM" format.
/// Returns the parsed NaiveDateTime if valid.
pub fn parse_event_datetime(s: &str) -> Option<chrono::NaiveDateTime> {
    // Strip trailing 'Z' and fractional seconds to normalize ISO 8601 variants
    let s = s.trim_end_matches('Z');
    let s = if let Some(dot) = s.rfind('.') {
        &s[..dot]
    } else {
        s
    };
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .ok()
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub async fn is_product_backed_by_event(
    pool: &SqlitePool,
    product_id: &str,
) -> anyhow::Result<bool> {
    let exists: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM events WHERE product_id = ? LIMIT 1")
            .bind(product_id)
            .fetch_optional(pool)
            .await?;

    Ok(exists.is_some())
}

pub async fn get_event_context_by_product(
    pool: &SqlitePool,
    product_id: &str,
) -> anyhow::Result<Option<EventContext>> {
    let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT title, event_date, event_location
         FROM events
         WHERE product_id = ? AND status IN ('draft', 'active', 'past')
         LIMIT 1",
    )
    .bind(product_id)
    .fetch_optional(pool)
    .await?;

    Ok(
        row.map(|(event_title, event_date, event_location)| EventContext {
            event_title,
            event_date,
            event_location,
        }),
    )
}

#[allow(dead_code)]
pub async fn create_event(
    pool: &SqlitePool,
    merchant_id: &str,
    product_id: &str,
    title: &str,
    description: Option<&str>,
    event_date: Option<&str>,
    event_location: Option<&str>,
) -> anyhow::Result<Event> {
    let id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO events (id, merchant_id, product_id, title, description, event_date, event_location, status)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'active')"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(product_id)
    .bind(title)
    .bind(description)
    .bind(event_date)
    .bind(event_location)
    .execute(pool)
    .await?;

    get_event(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Event not found after insert"))
}

#[allow(dead_code)]
pub async fn get_event(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Event>> {
    let row = sqlx::query_as::<_, Event>(
        "SELECT id, merchant_id, product_id, title, description, event_date, event_location, status, created_at
         FROM events
         WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn list_events_for_merchant(
    pool: &SqlitePool,
    merchant_id: &str,
) -> anyhow::Result<Vec<EventSummary>> {
    let rows = sqlx::query_as::<_, EventSummary>(
        "SELECT
            e.id, e.product_id, e.title, e.description, e.event_date, e.event_location, e.status, e.created_at,
            CASE WHEN e.luma_event_id IS NOT NULL
              THEN (SELECT COUNT(*) FROM invoices i WHERE i.product_id = e.product_id AND i.status = 'confirmed')
              ELSE (SELECT COUNT(*) FROM tickets t WHERE t.product_id = e.product_id AND t.status != 'void')
            END AS sold_count,
            CASE WHEN e.luma_event_id IS NOT NULL
              THEN 0
              ELSE (SELECT COUNT(*) FROM tickets t WHERE t.product_id = e.product_id AND t.status = 'used')
            END AS used_count,
            (SELECT CASE
               WHEN EXISTS (SELECT 1 FROM prices WHERE product_id = e.product_id AND active = 1 AND max_quantity IS NULL)
               THEN NULL
               ELSE (SELECT SUM(max_quantity) FROM prices WHERE product_id = e.product_id AND active = 1)
             END) AS total_capacity,
            e.luma_event_id
         FROM events e
         WHERE e.merchant_id = ?
         ORDER BY e.created_at DESC"
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn create_event_with_product_and_prices(
    pool: &SqlitePool,
    merchant_id: &str,
    req: &CreateEventRequest,
) -> anyhow::Result<Event> {
    if req.title.trim().is_empty() {
        anyhow::bail!("title is required");
    }
    if req.title.len() > 200 {
        anyhow::bail!("title must be 200 characters or fewer");
    }
    if let Some(ref d) = req.description {
        if d.len() > 2000 {
            anyhow::bail!("description must be 2000 characters or fewer");
        }
    }
    if let Some(ref loc) = req.event_location {
        if loc.len() > 300 {
            anyhow::bail!("event_location must be 300 characters or fewer");
        }
    }
    if let Some(ref date) = req.event_date {
        if date.len() > 30 {
            anyhow::bail!("event_date must be 30 characters or fewer");
        }
        if parse_event_datetime(date).is_none() {
            anyhow::bail!("event_date must be a valid date (YYYY-MM-DDTHH:MM)");
        }
    }
    if req.prices.is_empty() {
        anyhow::bail!("at least one price is required");
    }
    if req.prices.len() > 20 {
        anyhow::bail!("maximum 20 price tiers per event");
    }

    let mut tx = pool.begin().await?;

    let product_id = Uuid::new_v4().to_string();
    let slug = slugify(&req.title);

    sqlx::query(
        "INSERT INTO products (id, merchant_id, slug, name, description, default_price_id, metadata, active)
         VALUES (?, ?, ?, ?, ?, NULL, NULL, 1)"
    )
    .bind(&product_id)
    .bind(merchant_id)
    .bind(&slug)
    .bind(&req.title)
    .bind(&req.description)
    .execute(&mut *tx)
    .await?;

    let mut default_price_id: Option<String> = None;
    for p in &req.prices {
        let currency = p.currency.to_uppercase();
        if !crate::prices::SUPPORTED_CURRENCIES.contains(&currency.as_str()) {
            anyhow::bail!("Unsupported currency: {}", currency);
        }
        if p.unit_amount <= 0.0 {
            anyhow::bail!("unit_amount must be > 0");
        }
        if let Some(ref label) = p.label {
            if label.len() > 100 {
                anyhow::bail!("price label must be 100 characters or fewer");
            }
        }
        if let Some(max_q) = p.max_quantity {
            if max_q <= 0 {
                anyhow::bail!("max_quantity must be > 0");
            }
        }

        let price_id = format!("cprice_{}", Uuid::new_v4().to_string().replace('-', ""));
        if default_price_id.is_none() {
            default_price_id = Some(price_id.clone());
        }

        sqlx::query(
            "INSERT INTO prices (
                id, product_id, currency, unit_amount, label, max_quantity, price_type, billing_interval, interval_count, active
             ) VALUES (?, ?, ?, ?, ?, ?, 'one_time', NULL, NULL, 1)"
        )
        .bind(&price_id)
        .bind(&product_id)
        .bind(&currency)
        .bind(p.unit_amount)
        .bind(&p.label)
        .bind(p.max_quantity)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("UPDATE products SET default_price_id = ? WHERE id = ?")
        .bind(default_price_id)
        .bind(&product_id)
        .execute(&mut *tx)
        .await?;

    let event_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO events (id, merchant_id, product_id, title, description, event_date, event_location, status)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'active')"
    )
    .bind(&event_id)
    .bind(merchant_id)
    .bind(&product_id)
    .bind(&req.title)
    .bind(&req.description)
    .bind(&req.event_date)
    .bind(&req.event_location)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    get_event(pool, &event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Event not found after transactional create"))
}

pub async fn archive_event(
    pool: &SqlitePool,
    merchant_id: &str,
    event_id: &str,
) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;

    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, product_id FROM events WHERE id = ? AND merchant_id = ?")
            .bind(event_id)
            .bind(merchant_id)
            .fetch_optional(&mut *tx)
            .await?;

    let (_, product_id) = match row {
        Some(v) => v,
        None => return Ok(false),
    };

    sqlx::query("UPDATE events SET status = 'cancelled' WHERE id = ?")
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE products SET active = 0 WHERE id = ? AND merchant_id = ?")
        .bind(&product_id)
        .bind(merchant_id)
        .execute(&mut *tx)
        .await?;

    // Void all outstanding tickets for this event
    sqlx::query(
        "UPDATE tickets SET status = 'void'
         WHERE product_id = ? AND merchant_id = ? AND status = 'valid'",
    )
    .bind(&product_id)
    .bind(merchant_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

pub async fn update_event(
    pool: &SqlitePool,
    merchant_id: &str,
    event_id: &str,
    req: &UpdateEventRequest,
) -> anyhow::Result<Option<Event>> {
    let existing: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, product_id, status FROM events WHERE id = ? AND merchant_id = ?",
    )
    .bind(event_id)
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    let (_, product_id, status) = match existing {
        Some(v) => v,
        None => return Ok(None),
    };

    if status == "cancelled" {
        anyhow::bail!("cannot edit a cancelled event");
    }

    if let Some(ref t) = req.title {
        if t.trim().is_empty() {
            anyhow::bail!("title is required");
        }
        if t.len() > 200 {
            anyhow::bail!("title must be 200 characters or fewer");
        }
    }
    if let Some(ref d) = req.description {
        if d.len() > 2000 {
            anyhow::bail!("description must be 2000 characters or fewer");
        }
    }
    if let Some(ref loc) = req.event_location {
        if loc.len() > 300 {
            anyhow::bail!("event_location must be 300 characters or fewer");
        }
    }
    if let Some(ref date) = req.event_date {
        if date.len() > 30 {
            anyhow::bail!("event_date must be 30 characters or fewer");
        }
        if parse_event_datetime(date).is_none() {
            anyhow::bail!("event_date must be a valid date (YYYY-MM-DDTHH:MM)");
        }
    }

    let mut tx = pool.begin().await?;

    sqlx::query(
        "UPDATE events SET
            title = COALESCE(?, title),
            description = COALESCE(?, description),
            event_date = COALESCE(?, event_date),
            event_location = COALESCE(?, event_location)
         WHERE id = ? AND merchant_id = ?",
    )
    .bind(&req.title)
    .bind(&req.description)
    .bind(&req.event_date)
    .bind(&req.event_location)
    .bind(event_id)
    .bind(merchant_id)
    .execute(&mut *tx)
    .await?;

    if req.title.is_some() || req.description.is_some() {
        sqlx::query(
            "UPDATE products SET
                name = COALESCE(?, name),
                description = COALESCE(?, description)
             WHERE id = ? AND merchant_id = ?",
        )
        .bind(&req.title)
        .bind(&req.description)
        .bind(&product_id)
        .bind(merchant_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    get_event(pool, event_id).await
}

pub async fn get_event_tier_stats(
    pool: &SqlitePool,
    product_id: &str,
) -> anyhow::Result<Vec<EventTierStat>> {
    let is_luma: bool = sqlx::query_scalar::<_, Option<String>>(
        "SELECT luma_event_id FROM events WHERE product_id = ? LIMIT 1",
    )
    .bind(product_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .is_some();

    let rows = if is_luma {
        sqlx::query_as::<_, EventTierStat>(
            "SELECT
                pr.id AS price_id, pr.label, pr.currency, pr.unit_amount, pr.max_quantity,
                (SELECT COUNT(*) FROM invoices i WHERE i.price_id = pr.id AND i.status = 'confirmed') AS sold_count,
                0 AS used_count
             FROM prices pr
             WHERE pr.product_id = ? AND pr.active = 1
             ORDER BY pr.unit_amount ASC"
        )
        .bind(product_id)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, EventTierStat>(
            "SELECT
                pr.id AS price_id, pr.label, pr.currency, pr.unit_amount, pr.max_quantity,
                (SELECT COUNT(*) FROM tickets t WHERE t.price_id = pr.id AND t.status != 'void') AS sold_count,
                (SELECT COUNT(*) FROM tickets t WHERE t.price_id = pr.id AND t.status = 'used') AS used_count
             FROM prices pr
             WHERE pr.product_id = ? AND pr.active = 1
             ORDER BY pr.unit_amount ASC"
        )
        .bind(product_id)
        .fetch_all(pool)
        .await?
    };

    Ok(rows)
}

pub async fn get_event_detail(
    pool: &SqlitePool,
    merchant_id: &str,
    event_id: &str,
) -> anyhow::Result<Option<EventDetailResponse>> {
    let summary: Option<EventSummary> = sqlx::query_as::<_, EventSummary>(
        "SELECT
            e.id, e.product_id, e.title, e.description, e.event_date, e.event_location, e.status, e.created_at,
            (SELECT COUNT(*) FROM tickets t WHERE t.product_id = e.product_id AND t.status != 'void') AS sold_count,
            (SELECT COUNT(*) FROM tickets t WHERE t.product_id = e.product_id AND t.status = 'used') AS used_count,
            (SELECT CASE
               WHEN EXISTS (SELECT 1 FROM prices WHERE product_id = e.product_id AND active = 1 AND max_quantity IS NULL)
               THEN NULL
               ELSE (SELECT SUM(max_quantity) FROM prices WHERE product_id = e.product_id AND active = 1)
             END) AS total_capacity,
            e.luma_event_id
         FROM events e
         WHERE e.id = ? AND e.merchant_id = ?"
    )
    .bind(event_id)
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    let summary = match summary {
        Some(s) => s,
        None => return Ok(None),
    };

    let tiers = get_event_tier_stats(pool, &summary.product_id).await?;

    Ok(Some(EventDetailResponse { summary, tiers }))
}

pub async fn mark_past_events(pool: &SqlitePool) -> anyhow::Result<u64> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Also deactivate the backing products so no new checkouts can be created
    sqlx::query(
        "UPDATE products SET active = 0
         WHERE id IN (
             SELECT product_id FROM events
             WHERE status = 'active' AND event_date IS NOT NULL AND event_date < ?
         ) AND active = 1",
    )
    .bind(&now)
    .execute(pool)
    .await?;

    let result = sqlx::query(
        "UPDATE events
         SET status = 'past'
         WHERE status = 'active'
         AND event_date IS NOT NULL
         AND event_date < ?",
    )
    .bind(&now)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}
