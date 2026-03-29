use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PaymentLink {
    pub id: String,
    pub merchant_id: String,
    pub price_id: String,
    pub slug: String,
    pub name: Option<String>,
    pub success_url: Option<String>,
    pub metadata: Option<String>,
    pub active: i32,
    pub total_created: i32,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreatePaymentLinkRequest {
    pub price_id: String,
    pub name: Option<String>,
    pub success_url: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePaymentLinkRequest {
    pub name: Option<String>,
    pub success_url: Option<String>,
    pub active: Option<bool>,
    pub metadata: Option<serde_json::Value>,
}

impl PaymentLink {
    pub fn metadata_json(&self) -> Option<serde_json::Value> {
        self.metadata
            .as_ref()
            .and_then(|m| serde_json::from_str(m).ok())
    }
}

fn generate_slug() -> String {
    let id = Uuid::new_v4().to_string().replace('-', "");
    format!("pl_{}", &id[..12])
}

pub async fn create_payment_link(
    pool: &SqlitePool,
    merchant_id: &str,
    req: &CreatePaymentLinkRequest,
) -> anyhow::Result<PaymentLink> {
    let price = crate::prices::get_price(pool, &req.price_id).await?;
    let price = match price {
        Some(p) if p.active == 1 => p,
        Some(_) => anyhow::bail!("Price is not active"),
        None => anyhow::bail!("Price not found"),
    };

    let product = match crate::products::get_product(pool, &price.product_id).await? {
        Some(p) if p.merchant_id == merchant_id && p.active == 1 => p,
        Some(p) if p.merchant_id != merchant_id => anyhow::bail!("Price does not belong to this merchant"),
        Some(_) => anyhow::bail!("Product is not active"),
        None => anyhow::bail!("Product not found"),
    };

    if let Some(ref url) = req.success_url {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            anyhow::bail!("success_url must be a valid HTTP(S) URL");
        }
    }

    let id = Uuid::new_v4().to_string();
    let slug = generate_slug();
    let metadata_json = req.metadata.as_ref().map(|m| serde_json::to_string(m).unwrap_or_default());
    let display_name = match &req.name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => product.name.clone(),
    };

    sqlx::query(
        "INSERT INTO payment_links (id, merchant_id, price_id, slug, name, success_url, metadata)
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&req.price_id)
    .bind(&slug)
    .bind(&display_name)
    .bind(&req.success_url)
    .bind(&metadata_json)
    .execute(pool)
    .await?;

    tracing::info!(link_id = %id, slug = %slug, "Payment link created");

    get_payment_link(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Payment link not found after insert"))
}

pub async fn get_payment_link(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<PaymentLink>> {
    let row = sqlx::query_as::<_, PaymentLink>(
        "SELECT id, merchant_id, price_id, slug, name, success_url, metadata, active, total_created, created_at
         FROM payment_links WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn get_by_slug(pool: &SqlitePool, slug: &str) -> anyhow::Result<Option<PaymentLink>> {
    let row = sqlx::query_as::<_, PaymentLink>(
        "SELECT id, merchant_id, price_id, slug, name, success_url, metadata, active, total_created, created_at
         FROM payment_links WHERE slug = ?"
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn list_payment_links(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<Vec<PaymentLink>> {
    let rows = sqlx::query_as::<_, PaymentLink>(
        "SELECT id, merchant_id, price_id, slug, name, success_url, metadata, active, total_created, created_at
         FROM payment_links WHERE merchant_id = ?
         ORDER BY created_at DESC"
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn update_payment_link(
    pool: &SqlitePool,
    id: &str,
    merchant_id: &str,
    req: &UpdatePaymentLinkRequest,
) -> anyhow::Result<Option<PaymentLink>> {
    let existing = match get_payment_link(pool, id).await? {
        Some(pl) if pl.merchant_id == merchant_id => pl,
        Some(_) => anyhow::bail!("Payment link does not belong to this merchant"),
        None => return Ok(None),
    };

    if let Some(ref url) = req.success_url {
        if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
            anyhow::bail!("success_url must be a valid HTTP(S) URL");
        }
    }

    let name = req.name.as_deref().unwrap_or(existing.name.as_deref().unwrap_or(""));
    let success_url = req.success_url.as_ref().or(existing.success_url.as_ref());
    let active = req.active.map(|a| if a { 1 } else { 0 }).unwrap_or(existing.active);
    let metadata_json = req.metadata.as_ref()
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .or(existing.metadata);

    sqlx::query(
        "UPDATE payment_links SET name = ?, success_url = ?, active = ?, metadata = ?
         WHERE id = ? AND merchant_id = ?"
    )
    .bind(name)
    .bind(success_url)
    .bind(active)
    .bind(&metadata_json)
    .bind(id)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    tracing::info!(link_id = %id, "Payment link updated");
    get_payment_link(pool, id).await
}

pub async fn delete_payment_link(
    pool: &SqlitePool,
    id: &str,
    merchant_id: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "DELETE FROM payment_links WHERE id = ? AND merchant_id = ?"
    )
    .bind(id)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!(link_id = %id, "Payment link deleted");
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Atomically increment total_created counter for tracking
pub async fn increment_created(pool: &SqlitePool, id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE payment_links SET total_created = total_created + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
