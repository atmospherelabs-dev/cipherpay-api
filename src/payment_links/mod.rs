use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const PL_COLUMNS: &str = "id, merchant_id, price_id, slug, name, success_url, metadata, active, total_created, mode, donation_config, total_raised, created_at";

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PaymentLink {
    pub id: String,
    pub merchant_id: String,
    pub price_id: Option<String>,
    pub slug: String,
    pub name: Option<String>,
    pub success_url: Option<String>,
    pub metadata: Option<String>,
    pub active: i32,
    pub total_created: i32,
    pub mode: String,
    pub donation_config: Option<String>,
    pub total_raised: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DonationConfig {
    pub mission: Option<String>,
    pub thank_you: Option<String>,
    pub suggested_amounts: Option<Vec<i64>>,
    pub currency: String,
    pub min_amount: Option<i64>,
    pub max_amount: Option<i64>,
    pub campaign_name: Option<String>,
    pub campaign_goal: Option<i64>,
    pub cover_image_url: Option<String>,
    pub cover_image_position: Option<String>,
    pub contact_email: Option<String>,
    pub website_url: Option<String>,
    pub social_share_text: Option<String>,
}

impl DonationConfig {
    pub fn effective_min(&self) -> i64 {
        self.min_amount.unwrap_or(100) // $1 in cents
    }

    pub fn effective_max(&self) -> i64 {
        self.max_amount.unwrap_or(1_000_000) // $10,000 in cents
    }
}

#[derive(Debug, Deserialize)]
pub struct CreatePaymentLinkRequest {
    pub price_id: String,
    pub name: Option<String>,
    pub success_url: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDonationLinkRequest {
    pub name: String,
    pub mission: Option<String>,
    pub thank_you: Option<String>,
    pub suggested_amounts: Option<Vec<i64>>,
    pub currency: Option<String>,
    pub min_amount: Option<i64>,
    pub max_amount: Option<i64>,
    pub campaign_name: Option<String>,
    pub campaign_goal: Option<i64>,
    pub cover_image_url: Option<String>,
    pub cover_image_position: Option<String>,
    pub contact_email: Option<String>,
    pub website_url: Option<String>,
    pub social_share_text: Option<String>,
    pub success_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePaymentLinkRequest {
    pub name: Option<String>,
    pub success_url: Option<String>,
    pub active: Option<bool>,
    pub metadata: Option<serde_json::Value>,
    pub donation_config: Option<DonationConfig>,
}

impl PaymentLink {
    pub fn metadata_json(&self) -> Option<serde_json::Value> {
        self.metadata
            .as_ref()
            .and_then(|m| serde_json::from_str(m).ok())
    }

    pub fn donation_config_parsed(&self) -> Option<DonationConfig> {
        self.donation_config
            .as_ref()
            .and_then(|c| serde_json::from_str(c).ok())
    }

    pub fn is_donation(&self) -> bool {
        self.mode == "donation"
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
        "INSERT INTO payment_links (id, merchant_id, price_id, slug, name, success_url, metadata, mode)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'payment')"
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

pub async fn create_donation_link(
    pool: &SqlitePool,
    merchant_id: &str,
    req: &CreateDonationLinkRequest,
) -> anyhow::Result<PaymentLink> {
    if req.name.trim().is_empty() {
        anyhow::bail!("name is required for donation links");
    }

    if let Some(ref url) = req.cover_image_url {
        if !url.starts_with("https://") {
            anyhow::bail!("cover_image_url must be an HTTPS URL");
        }
    }
    if let Some(ref url) = req.website_url {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            anyhow::bail!("website_url must be a valid HTTP(S) URL");
        }
    }
    if let Some(ref url) = req.success_url {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            anyhow::bail!("success_url must be a valid HTTP(S) URL");
        }
    }

    if let (Some(_name), None) = (&req.campaign_name, &req.campaign_goal) {
        anyhow::bail!("campaign_goal is required when campaign_name is set");
    }

    let config = DonationConfig {
        mission: req.mission.clone(),
        thank_you: req.thank_you.clone(),
        suggested_amounts: req.suggested_amounts.clone(),
        currency: req.currency.clone().unwrap_or_else(|| "USD".to_string()),
        min_amount: req.min_amount,
        max_amount: req.max_amount,
        campaign_name: req.campaign_name.clone(),
        campaign_goal: req.campaign_goal,
        cover_image_url: req.cover_image_url.clone(),
        cover_image_position: req.cover_image_position.clone(),
        contact_email: req.contact_email.clone(),
        website_url: req.website_url.clone(),
        social_share_text: req.social_share_text.clone(),
    };

    let config_json = serde_json::to_string(&config)?;
    let id = Uuid::new_v4().to_string();
    let slug = generate_slug();

    sqlx::query(
        "INSERT INTO payment_links (id, merchant_id, slug, name, success_url, mode, donation_config)
         VALUES (?, ?, ?, ?, ?, 'donation', ?)"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&slug)
    .bind(req.name.trim())
    .bind(&req.success_url)
    .bind(&config_json)
    .execute(pool)
    .await?;

    tracing::info!(link_id = %id, slug = %slug, "Donation link created");

    get_payment_link(pool, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Donation link not found after insert"))
}

pub async fn get_payment_link(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<PaymentLink>> {
    let query = format!("SELECT {} FROM payment_links WHERE id = ?", PL_COLUMNS);
    let row = sqlx::query_as::<_, PaymentLink>(&query)
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(row)
}

pub async fn get_by_slug(pool: &SqlitePool, slug: &str) -> anyhow::Result<Option<PaymentLink>> {
    let query = format!("SELECT {} FROM payment_links WHERE slug = ?", PL_COLUMNS);
    let row = sqlx::query_as::<_, PaymentLink>(&query)
        .bind(slug)
        .fetch_optional(pool)
        .await?;

    Ok(row)
}

pub async fn list_payment_links(pool: &SqlitePool, merchant_id: &str) -> anyhow::Result<Vec<PaymentLink>> {
    let query = format!(
        "SELECT {} FROM payment_links WHERE merchant_id = ? ORDER BY created_at DESC",
        PL_COLUMNS
    );
    let rows = sqlx::query_as::<_, PaymentLink>(&query)
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

    let donation_config_json = if existing.mode == "donation" {
        req.donation_config.as_ref()
            .map(|dc| serde_json::to_string(dc).unwrap_or_default())
            .or(existing.donation_config)
    } else {
        existing.donation_config
    };

    sqlx::query(
        "UPDATE payment_links SET name = ?, success_url = ?, active = ?, metadata = ?, donation_config = ?
         WHERE id = ? AND merchant_id = ?"
    )
    .bind(name)
    .bind(success_url)
    .bind(active)
    .bind(&metadata_json)
    .bind(&donation_config_json)
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

/// Atomically increment total_raised by fiat cents. Used when a donation invoice is confirmed.
pub async fn increment_raised(pool: &SqlitePool, id: &str, amount_cents: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE payment_links SET total_raised = total_raised + ? WHERE id = ?")
        .bind(amount_cents)
        .bind(id)
        .execute(pool)
        .await?;
    tracing::info!(link_id = %id, amount_cents, "Campaign total_raised incremented");
    Ok(())
}
