use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Product {
    pub id: String,
    pub merchant_id: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub default_price_id: Option<String>,
    pub metadata: Option<String>,
    pub active: i32,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateProductRequest {
    pub slug: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub unit_amount: f64,
    pub currency: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub price_type: Option<String>,
    pub billing_interval: Option<String>,
    pub interval_count: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProductRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub default_price_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub active: Option<bool>,
}

impl Product {
    pub fn metadata_json(&self) -> Option<serde_json::Value> {
        self.metadata
            .as_ref()
            .and_then(|m| serde_json::from_str(m).ok())
    }
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

pub async fn create_product(
    pool: &SqlitePool,
    merchant_id: &str,
    req: &CreateProductRequest,
) -> anyhow::Result<Product> {
    if req.name.is_empty() || req.unit_amount <= 0.0 {
        anyhow::bail!("name required and price must be > 0");
    }

    let slug = match &req.slug {
        Some(s) if !s.is_empty() => {
            if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                anyhow::bail!("slug must only contain letters, numbers, underscores, hyphens");
            }
            s.clone()
        }
        _ => slugify(&req.name),
    };

    let currency = req.currency.as_deref().unwrap_or("EUR");
    if !crate::prices::SUPPORTED_CURRENCIES.contains(&currency) {
        anyhow::bail!("Unsupported currency: {}", currency);
    }

    let id = Uuid::new_v4().to_string();
    let metadata_json = req.metadata.as_ref().map(|m| serde_json::to_string(m).unwrap_or_default());

    sqlx::query(
        "INSERT INTO products (id, merchant_id, slug, name, description, default_price_id, metadata)
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&slug)
    .bind(&req.name)
    .bind(&req.description)
    .bind::<Option<String>>(None)
    .bind(&metadata_json)
    .execute(pool)
    .await?;

    tracing::info!(product_id = %id, slug = %slug, "Product created");

    get_product(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Product not found after insert"))
}

/// Set the product's default_price_id (used after creating the initial Price).
pub async fn set_default_price(
    pool: &SqlitePool,
    product_id: &str,
    price_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE products SET default_price_id = ? WHERE id = ?")
        .bind(price_id)
        .bind(product_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_products(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<Vec<Product>> {
    let rows = sqlx::query_as::<_, Product>(
        "SELECT id, merchant_id, slug, name, description, default_price_id, metadata, active, created_at
         FROM products WHERE merchant_id = ? AND active = 1 ORDER BY created_at DESC"
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_product(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Product>> {
    let row = sqlx::query_as::<_, Product>(
        "SELECT id, merchant_id, slug, name, description, default_price_id, metadata, active, created_at
         FROM products WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn update_product(
    pool: &SqlitePool,
    id: &str,
    merchant_id: &str,
    req: &UpdateProductRequest,
) -> anyhow::Result<Option<Product>> {
    let existing = match get_product(pool, id).await? {
        Some(p) if p.merchant_id == merchant_id => p,
        Some(_) => anyhow::bail!("Product does not belong to this merchant"),
        None => return Ok(None),
    };

    let name = req.name.as_deref().unwrap_or(&existing.name);
    let description = req.description.as_ref().or(existing.description.as_ref());
    let default_price_id = req.default_price_id.as_ref().or(existing.default_price_id.as_ref());
    let active = req.active.map(|a| if a { 1 } else { 0 }).unwrap_or(existing.active);
    let metadata_json = req.metadata.as_ref()
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .or(existing.metadata);

    if let Some(price_id) = default_price_id {
        let price = crate::prices::get_price(pool, price_id).await?;
        match price {
            Some(p) if p.product_id == id && p.active == 1 => {}
            Some(_) => anyhow::bail!("default_price_id must reference an active price belonging to this product"),
            None => anyhow::bail!("default_price_id references a non-existent price"),
        }
    }

    sqlx::query(
        "UPDATE products SET name = ?, description = ?, default_price_id = ?, metadata = ?, active = ?
         WHERE id = ? AND merchant_id = ?"
    )
    .bind(name)
    .bind(description)
    .bind(default_price_id)
    .bind(&metadata_json)
    .bind(active)
    .bind(id)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    tracing::info!(product_id = %id, "Product updated");
    get_product(pool, id).await
}

/// Stripe-style delete: hard-delete if no invoices reference this product,
/// otherwise archive (set active = 0).
pub async fn delete_product(
    pool: &SqlitePool,
    id: &str,
    merchant_id: &str,
) -> anyhow::Result<DeleteOutcome> {
    match get_product(pool, id).await? {
        Some(p) if p.merchant_id == merchant_id => {}
        Some(_) => anyhow::bail!("Product does not belong to this merchant"),
        None => return Ok(DeleteOutcome::NotFound),
    };

    let invoice_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM invoices WHERE product_id = ?"
    )
    .bind(id)
    .fetch_one(pool)
    .await?;

    if invoice_count.0 == 0 {
        sqlx::query("DELETE FROM prices WHERE product_id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM products WHERE id = ? AND merchant_id = ?")
            .bind(id)
            .bind(merchant_id)
            .execute(pool)
            .await?;
        tracing::info!(product_id = %id, "Product hard-deleted (no invoices)");
        Ok(DeleteOutcome::Deleted)
    } else {
        sqlx::query("UPDATE products SET active = 0 WHERE id = ? AND merchant_id = ?")
            .bind(id)
            .bind(merchant_id)
            .execute(pool)
            .await?;
        tracing::info!(product_id = %id, invoices = invoice_count.0, "Product archived (has invoices)");
        Ok(DeleteOutcome::Archived)
    }
}

pub enum DeleteOutcome {
    Deleted,
    Archived,
    NotFound,
}
