use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Ticket {
    pub id: String,
    pub invoice_id: String,
    pub product_id: String,
    pub price_id: Option<String>,
    pub merchant_id: String,
    pub code: String,
    pub status: String,
    pub used_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TicketListItem {
    pub id: String,
    pub invoice_id: String,
    pub code: String,
    pub status: String,
    pub used_at: Option<String>,
    pub created_at: String,
    pub product_id: String,
    pub product_name: Option<String>,
    pub price_id: Option<String>,
    pub price_label: Option<String>,
    pub event_title: Option<String>,
    pub event_date: Option<String>,
    pub event_location: Option<String>,
}

fn generate_ticket_code() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!("tkt_{}", hex::encode(bytes))
}

pub async fn create_ticket(
    pool: &SqlitePool,
    invoice_id: &str,
    product_id: &str,
    price_id: Option<&str>,
    merchant_id: &str,
) -> anyhow::Result<Option<Ticket>> {
    // Retry loop handles the astronomically unlikely case of a code collision
    for attempt in 0..3 {
        let id = Uuid::new_v4().to_string();
        let code = generate_ticket_code();

        let result = sqlx::query(
            "INSERT OR IGNORE INTO tickets (id, invoice_id, product_id, price_id, merchant_id, code, status)
             VALUES (?, ?, ?, ?, ?, ?, 'valid')"
        )
        .bind(&id)
        .bind(invoice_id)
        .bind(product_id)
        .bind(price_id)
        .bind(merchant_id)
        .bind(&code)
        .execute(pool)
        .await?;

        if result.rows_affected() == 0 {
            // Idempotent: ticket already exists for this invoice
            if let Some(existing) = get_ticket_by_invoice(pool, invoice_id).await? {
                return Ok(Some(existing));
            }
            // No ticket for this invoice — likely a code collision; retry
            tracing::warn!(
                invoice_id,
                attempt,
                "Ticket insert ignored but no existing ticket found, retrying with new code"
            );
            continue;
        }

        return get_ticket(pool, &id).await;
    }

    anyhow::bail!("Failed to create ticket after retries (code collision)")
}

pub async fn get_ticket(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Ticket>> {
    let row = sqlx::query_as::<_, Ticket>(
        "SELECT id, invoice_id, product_id, price_id, merchant_id, code, status, used_at, created_at
         FROM tickets WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_ticket_by_invoice(
    pool: &SqlitePool,
    invoice_id: &str,
) -> anyhow::Result<Option<Ticket>> {
    let row = sqlx::query_as::<_, Ticket>(
        "SELECT id, invoice_id, product_id, price_id, merchant_id, code, status, used_at, created_at
         FROM tickets WHERE invoice_id = ?"
    )
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_ticket_by_code(pool: &SqlitePool, code: &str) -> anyhow::Result<Option<Ticket>> {
    let row = sqlx::query_as::<_, Ticket>(
        "SELECT id, invoice_id, product_id, price_id, merchant_id, code, status, used_at, created_at
         FROM tickets WHERE code = ?"
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn scan_ticket(
    pool: &SqlitePool,
    code: &str,
    merchant_id: &str,
) -> anyhow::Result<Option<(Ticket, bool)>> {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let update = sqlx::query(
        "UPDATE tickets
         SET status = 'used', used_at = ?
         WHERE code = ? AND merchant_id = ? AND status = 'valid'",
    )
    .bind(&now)
    .bind(code)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    let ticket = get_ticket_by_code(pool, code).await?;
    match ticket {
        Some(t) => Ok(Some((t, update.rows_affected() > 0))),
        None => Ok(None),
    }
}

pub async fn void_ticket(
    pool: &SqlitePool,
    ticket_id: &str,
    merchant_id: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tickets
         SET status = 'void'
         WHERE id = ? AND merchant_id = ? AND status != 'used'",
    )
    .bind(ticket_id)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn list_tickets_for_merchant(
    pool: &SqlitePool,
    merchant_id: &str,
) -> anyhow::Result<Vec<TicketListItem>> {
    let rows = sqlx::query_as::<_, TicketListItem>(
        "SELECT
            t.id, t.invoice_id, t.code, t.status, t.used_at, t.created_at,
            t.product_id, p.name AS product_name,
            t.price_id, pr.label AS price_label,
            e.title AS event_title, e.event_date, e.event_location
         FROM tickets t
         LEFT JOIN products p ON p.id = t.product_id
         LEFT JOIN prices pr ON pr.id = t.price_id
         LEFT JOIN events e ON e.product_id = t.product_id
         WHERE t.merchant_id = ?
         ORDER BY t.created_at DESC",
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
