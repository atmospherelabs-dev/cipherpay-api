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
    pub price_eur: f64,
    pub currency: String,
    pub variants: Option<String>,
    pub active: i32,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateProductRequest {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub price_eur: f64,
    pub currency: Option<String>,
    pub variants: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProductRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub price_eur: Option<f64>,
    pub currency: Option<String>,
    pub variants: Option<Vec<String>>,
    pub active: Option<bool>,
}

impl Product {
    pub fn variants_list(&self) -> Vec<String> {
        self.variants
            .as_ref()
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default()
    }
}

pub async fn create_product(
    pool: &SqlitePool,
    merchant_id: &str,
    req: &CreateProductRequest,
) -> anyhow::Result<Product> {
    if req.slug.is_empty() || req.name.is_empty() || req.price_eur <= 0.0 {
        anyhow::bail!("slug, name required and price must be > 0");
    }

    if !req.slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        anyhow::bail!("slug must only contain letters, numbers, underscores, hyphens");
    }

    let currency = req.currency.as_deref().unwrap_or("EUR");
    if currency != "EUR" && currency != "USD" {
        anyhow::bail!("currency must be EUR or USD");
    }

    let id = Uuid::new_v4().to_string();
    let variants_json = req.variants.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default());

    sqlx::query(
        "INSERT INTO products (id, merchant_id, slug, name, description, price_eur, currency, variants)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&req.slug)
    .bind(&req.name)
    .bind(&req.description)
    .bind(req.price_eur)
    .bind(currency)
    .bind(&variants_json)
    .execute(pool)
    .await?;

    tracing::info!(product_id = %id, slug = %req.slug, "Product created");

    get_product(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Product not found after insert"))
}

pub async fn list_products(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<Vec<Product>> {
    let rows = sqlx::query_as::<_, Product>(
        "SELECT id, merchant_id, slug, name, description, price_eur, currency, variants, active, created_at
         FROM products WHERE merchant_id = ? ORDER BY created_at DESC"
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_product(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Product>> {
    let row = sqlx::query_as::<_, Product>(
        "SELECT id, merchant_id, slug, name, description, price_eur, currency, variants, active, created_at
         FROM products WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn get_product_by_slug(
    pool: &SqlitePool,
    merchant_id: &str,
    slug: &str,
) -> anyhow::Result<Option<Product>> {
    let row = sqlx::query_as::<_, Product>(
        "SELECT id, merchant_id, slug, name, description, price_eur, currency, variants, active, created_at
         FROM products WHERE merchant_id = ? AND slug = ?"
    )
    .bind(merchant_id)
    .bind(slug)
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
    let price_eur = req.price_eur.unwrap_or(existing.price_eur);
    let currency = req.currency.as_deref().unwrap_or(&existing.currency);
    if currency != "EUR" && currency != "USD" {
        anyhow::bail!("currency must be EUR or USD");
    }
    let active = req.active.map(|a| if a { 1 } else { 0 }).unwrap_or(existing.active);
    let variants_json = req.variants.as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .or(existing.variants);

    if price_eur <= 0.0 {
        anyhow::bail!("Price must be > 0");
    }

    sqlx::query(
        "UPDATE products SET name = ?, description = ?, price_eur = ?, currency = ?, variants = ?, active = ?
         WHERE id = ? AND merchant_id = ?"
    )
    .bind(name)
    .bind(description)
    .bind(price_eur)
    .bind(currency)
    .bind(&variants_json)
    .bind(active)
    .bind(id)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    tracing::info!(product_id = %id, "Product updated");
    get_product(pool, id).await
}

pub async fn deactivate_product(
    pool: &SqlitePool,
    id: &str,
    merchant_id: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE products SET active = 0 WHERE id = ? AND merchant_id = ?"
    )
    .bind(id)
    .bind(merchant_id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!(product_id = %id, "Product deactivated");
        Ok(true)
    } else {
        Ok(false)
    }
}
