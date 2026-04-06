use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::payment_links::{self, CreatePaymentLinkRequest, CreateDonationLinkRequest, UpdatePaymentLinkRequest};
use crate::validation;

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreatePaymentLinkRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_merchant_or_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    if let Err(e) = validate_create(&body) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    match payment_links::create_payment_link(pool.get_ref(), &merchant.id, &body).await {
        Ok(link) => HttpResponse::Created().json(link_response(&link)),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create payment link");
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": e.to_string()
            }))
        }
    }
}

pub async fn create_donation(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreateDonationLinkRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_merchant_or_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    if let Err(e) = validate_create_donation(&body) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    match payment_links::create_donation_link(pool.get_ref(), &merchant.id, &body).await {
        Ok(link) => HttpResponse::Created().json(link_response(&link)),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create donation link");
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": e.to_string()
            }))
        }
    }
}

pub async fn list(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_merchant_or_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    match payment_links::list_payment_links(pool.get_ref(), &merchant.id).await {
        Ok(links) => {
            let mut result = Vec::with_capacity(links.len());
            for link in &links {
                let mut resp = link_response(link);
                if link.is_donation() {
                    let confirmed: i32 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM invoices WHERE payment_link_id = ? AND is_donation = 1 AND campaign_counted = 1"
                    )
                    .bind(&link.id)
                    .fetch_one(pool.get_ref())
                    .await
                    .unwrap_or(0);
                    resp["total_confirmed"] = serde_json::json!(confirmed);
                }
                result.push(resp);
            }
            HttpResponse::Ok().json(result)
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list payment links");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn update(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
    body: web::Json<UpdatePaymentLinkRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_merchant_or_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let link_id = path.into_inner();

    if let Err(e) = validate_update(&body) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    match payment_links::update_payment_link(pool.get_ref(), &link_id, &merchant.id, &body).await {
        Ok(Some(link)) => HttpResponse::Ok().json(link_response(&link)),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Payment link not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update payment link");
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": e.to_string()
            }))
        }
    }
}

pub async fn delete(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_merchant_or_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let link_id = path.into_inner();

    match payment_links::delete_payment_link(pool.get_ref(), &link_id, &merchant.id).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({ "status": "deleted" })),
        Ok(false) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Payment link not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete payment link");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

/// Public endpoint: return donation link info without creating an invoice.
/// Powers the /donate/{slug} amount selection page.
pub async fn info(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    path: web::Path<String>,
) -> HttpResponse {
    let slug = path.into_inner();

    let link = match payment_links::get_by_slug(pool.get_ref(), &slug).await {
        Ok(Some(l)) if l.active == 1 => l,
        Ok(Some(_)) => {
            return HttpResponse::Gone().json(serde_json::json!({
                "error": "This link is no longer active"
            }));
        }
        Ok(None) => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Link not found"
            }));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch link info");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };

    let merchant_name: Option<String> = sqlx::query_scalar(
        "SELECT name FROM merchants WHERE id = ?"
    )
    .bind(&link.merchant_id)
    .fetch_optional(pool.get_ref())
    .await
    .ok()
    .flatten();

    let frontend_url = config.frontend_url.as_deref().unwrap_or("https://cipherpay.app");

    let mut response = serde_json::json!({
        "slug": link.slug,
        "name": link.name,
        "mode": link.mode,
        "active": link.active == 1,
        "total_raised": link.total_raised,
        "total_created": link.total_created,
        "merchant_name": merchant_name,
    });

    if link.is_donation() {
        if let Some(config) = link.donation_config_parsed() {
            response["donation_config"] = serde_json::json!({
                "mission": config.mission,
                "thank_you": config.thank_you,
                "suggested_amounts": config.suggested_amounts,
                "currency": config.currency,
                "min_amount": config.effective_min(),
                "max_amount": config.effective_max(),
                "campaign_name": config.campaign_name,
                "campaign_goal": config.campaign_goal,
                "cover_image_url": config.cover_image_url,
                "cover_image_position": config.cover_image_position,
                "contact_email": config.contact_email,
                "website_url": config.website_url,
                "social_share_text": config.social_share_text,
            });
        }
        response["donate_url"] = serde_json::json!(format!("{}/en/donate/{}", frontend_url, link.slug));
    } else {
        response["checkout_url"] = serde_json::json!(format!("{}/link/{}", frontend_url, link.slug));
    }

    HttpResponse::Ok().json(response)
}

/// Public endpoint: resolve a payment link by slug and create an invoice.
/// Rate limited to prevent invoice flooding.
/// For donation links, amount + currency come from the request body.
pub async fn resolve(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
    path: web::Path<String>,
    body: web::Json<serde_json::Value>,
) -> HttpResponse {
    let slug = path.into_inner();

    let link = match payment_links::get_by_slug(pool.get_ref(), &slug).await {
        Ok(Some(l)) if l.active == 1 => l,
        Ok(Some(_)) => {
            return HttpResponse::Gone().json(serde_json::json!({
                "error": "This payment link is no longer active"
            }));
        }
        Ok(None) => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Payment link not found"
            }));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve payment link");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };

    if link.is_donation() {
        resolve_donation(pool, config, price_service, &link, &body).await
    } else {
        resolve_payment(pool, config, price_service, &link).await
    }
}

async fn resolve_donation(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
    link: &payment_links::PaymentLink,
    body: &serde_json::Value,
) -> HttpResponse {
    let donation_config = match link.donation_config_parsed() {
        Some(c) => c,
        None => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Donation link has invalid configuration"
            }));
        }
    };

    let amount_cents = match body.get("amount").and_then(|v| v.as_i64()) {
        Some(a) => a,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "amount is required (in cents)"
            }));
        }
    };

    let currency = body.get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or(&donation_config.currency);

    if amount_cents < donation_config.effective_min() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Minimum donation is {} cents", donation_config.effective_min())
        }));
    }
    if amount_cents > donation_config.effective_max() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Maximum donation is {} cents", donation_config.effective_max())
        }));
    }

    let merchant = match crate::merchants::get_merchant_by_id(
        pool.get_ref(), &link.merchant_id, &config.encryption_key
    ).await {
        Ok(Some(m)) => m,
        _ => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Merchant not found"
            }));
        }
    };

    if config.fee_enabled() {
        if let Ok(status) = crate::billing::get_merchant_billing_status(pool.get_ref(), &merchant.id).await {
            if status == "past_due" || status == "suspended" {
                return HttpResponse::PaymentRequired().json(serde_json::json!({
                    "error": "Merchant account has outstanding fees"
                }));
            }
        }
    }

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch ZEC rate for donation");
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Price feed unavailable"
            }));
        }
    };

    let amount_fiat = amount_cents as f64 / 100.0;
    let org_label = if merchant.name.is_empty() {
        link.name.as_deref().unwrap_or("Organization")
    } else {
        &merchant.name
    };
    let display_name = format!("Donation to {}", org_label);

    let invoice_req = crate::invoices::CreateInvoiceRequest {
        product_id: None,
        price_id: None,
        product_name: Some(display_name),
        size: None,
        amount: amount_fiat,
        currency: Some(currency.to_string()),
        refund_address: None,
    };

    let fee_config = if config.fee_enabled() {
        config.fee_address.as_ref().map(|addr| crate::invoices::FeeConfig {
            fee_address: addr.clone(),
            fee_rate: config.fee_rate,
        })
    } else {
        None
    };

    match crate::invoices::create_invoice(
        pool.get_ref(),
        &merchant.id,
        &merchant.ufvk,
        &invoice_req,
        &rates,
        config.invoice_expiry_minutes,
        fee_config.as_ref(),
    )
    .await
    {
        Ok(resp) => {
            let _ = payment_links::increment_created(pool.get_ref(), &link.id).await;

            // Tag the invoice as a donation and link it to the campaign
            sqlx::query(
                "UPDATE invoices SET payment_link_id = ?, is_donation = 1 WHERE id = ?"
            )
            .bind(&link.id)
            .bind(&resp.invoice_id)
            .execute(pool.get_ref())
            .await
            .ok();

            let frontend_url = config.frontend_url.as_deref().unwrap_or("https://cipherpay.app");
            let checkout_url = format!("{}/pay/{}", frontend_url, resp.invoice_id);

            HttpResponse::Created().json(serde_json::json!({
                "invoice_id": resp.invoice_id,
                "checkout_url": checkout_url,
                "payment_address": resp.payment_address,
                "amount": resp.amount,
                "currency": resp.currency,
                "price_zec": resp.price_zec,
                "zcash_uri": resp.zcash_uri,
                "expires_at": resp.expires_at,
                "is_donation": true,
                "link_name": link.name,
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, slug = %link.slug, "Donation invoice creation failed");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create invoice"
            }))
        }
    }
}

async fn resolve_payment(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
    link: &payment_links::PaymentLink,
) -> HttpResponse {
    let price_id = match &link.price_id {
        Some(pid) => pid,
        None => {
            return HttpResponse::Gone().json(serde_json::json!({
                "error": "Payment link has no price configured"
            }));
        }
    };

    let price = match crate::prices::get_price(pool.get_ref(), price_id).await {
        Ok(Some(p)) if p.active == 1 => p,
        _ => {
            return HttpResponse::Gone().json(serde_json::json!({
                "error": "Price associated with this link is no longer active"
            }));
        }
    };

    let product = match crate::products::get_product(pool.get_ref(), &price.product_id).await {
        Ok(Some(p)) if p.active == 1 => p,
        _ => {
            return HttpResponse::Gone().json(serde_json::json!({
                "error": "Product associated with this link is no longer available"
            }));
        }
    };

    let merchant = match crate::merchants::get_merchant_by_id(
        pool.get_ref(), &link.merchant_id, &config.encryption_key
    ).await {
        Ok(Some(m)) => m,
        _ => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Merchant not found"
            }));
        }
    };

    if config.fee_enabled() {
        if let Ok(status) = crate::billing::get_merchant_billing_status(pool.get_ref(), &merchant.id).await {
            if status == "past_due" || status == "suspended" {
                return HttpResponse::PaymentRequired().json(serde_json::json!({
                    "error": "Merchant account has outstanding fees"
                }));
            }
        }
    }

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch ZEC rate for payment link");
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Price feed unavailable"
            }));
        }
    };

    let invoice_req = crate::invoices::CreateInvoiceRequest {
        product_id: Some(product.id.clone()),
        price_id: Some(price.id.clone()),
        product_name: Some(product.name.clone()),
        size: None,
        amount: price.unit_amount,
        currency: Some(price.currency.clone()),
        refund_address: None,
    };

    let fee_config = if config.fee_enabled() {
        config.fee_address.as_ref().map(|addr| crate::invoices::FeeConfig {
            fee_address: addr.clone(),
            fee_rate: config.fee_rate,
        })
    } else {
        None
    };

    match crate::invoices::create_invoice(
        pool.get_ref(),
        &merchant.id,
        &merchant.ufvk,
        &invoice_req,
        &rates,
        config.invoice_expiry_minutes,
        fee_config.as_ref(),
    )
    .await
    {
        Ok(resp) => {
            let _ = payment_links::increment_created(pool.get_ref(), &link.id).await;

            // Tag with payment_link_id for tracking
            sqlx::query(
                "UPDATE invoices SET payment_link_id = ? WHERE id = ?"
            )
            .bind(&link.id)
            .bind(&resp.invoice_id)
            .execute(pool.get_ref())
            .await
            .ok();

            let frontend_url = config.frontend_url.as_deref().unwrap_or("https://cipherpay.app");
            let mut checkout_url = format!("{}/pay/{}", frontend_url, resp.invoice_id);
            if let Some(ref success) = link.success_url {
                let encoded: String = success.chars().map(|c| match c {
                    '&' | '=' | '?' | '#' | ' ' => format!("%{:02X}", c as u8),
                    _ => c.to_string(),
                }).collect();
                checkout_url = format!("{}?return_url={}", checkout_url, encoded);
            }

            HttpResponse::Created().json(serde_json::json!({
                "invoice_id": resp.invoice_id,
                "checkout_url": checkout_url,
                "payment_address": resp.payment_address,
                "amount": resp.amount,
                "currency": resp.currency,
                "price_zec": resp.price_zec,
                "zcash_uri": resp.zcash_uri,
                "expires_at": resp.expires_at,
                "product_name": product.name,
                "link_name": link.name,
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, slug = %link.slug, "Payment link invoice creation failed");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create invoice"
            }))
        }
    }
}

fn link_response(link: &payment_links::PaymentLink) -> serde_json::Value {
    let mut resp = serde_json::json!({
        "id": link.id,
        "merchant_id": link.merchant_id,
        "price_id": link.price_id,
        "slug": link.slug,
        "name": link.name,
        "success_url": link.success_url,
        "metadata": link.metadata_json(),
        "active": link.active == 1,
        "total_created": link.total_created,
        "mode": link.mode,
        "total_raised": link.total_raised,
        "created_at": link.created_at,
    });

    if link.is_donation() {
        if let Some(config) = link.donation_config_parsed() {
            resp["donation_config"] = serde_json::json!(config);
        }
    }

    resp
}

fn validate_create(req: &CreatePaymentLinkRequest) -> Result<(), validation::ValidationError> {
    validation::validate_length("price_id", &req.price_id, 100)?;
    if let Some(ref name) = req.name {
        validation::validate_length("name", name, 200)?;
    }
    if let Some(ref url) = req.success_url {
        validation::validate_length("success_url", url, 2000)?;
    }
    Ok(())
}

fn validate_create_donation(req: &CreateDonationLinkRequest) -> Result<(), validation::ValidationError> {
    validation::validate_length("name", &req.name, 200)?;
    if let Some(ref mission) = req.mission {
        validation::validate_length("mission", mission, 2000)?;
    }
    if let Some(ref ty) = req.thank_you {
        validation::validate_length("thank_you", ty, 2000)?;
    }
    if let Some(ref cn) = req.campaign_name {
        validation::validate_length("campaign_name", cn, 200)?;
    }
    if let Some(ref url) = req.cover_image_url {
        validation::validate_length("cover_image_url", url, 2000)?;
        validation::validate_url_protocol("cover_image_url", url, false)?;
    }
    if let Some(ref pos) = req.cover_image_position {
        validation::validate_image_position("cover_image_position", pos)?;
    }
    if let Some(ref email) = req.contact_email {
        validation::validate_length("contact_email", email, 200)?;
    }
    if let Some(ref url) = req.website_url {
        validation::validate_length("website_url", url, 2000)?;
        validation::validate_url_protocol("website_url", url, true)?;
    }
    if let Some(ref text) = req.social_share_text {
        validation::validate_length("social_share_text", text, 500)?;
    }
    if let Some(ref url) = req.success_url {
        validation::validate_length("success_url", url, 2000)?;
        validation::validate_url_protocol("success_url", url, true)?;
    }
    Ok(())
}

fn validate_update(req: &UpdatePaymentLinkRequest) -> Result<(), validation::ValidationError> {
    if let Some(ref name) = req.name {
        validation::validate_length("name", name, 200)?;
    }
    if let Some(ref url) = req.success_url {
        validation::validate_length("success_url", url, 2000)?;
        if !url.is_empty() {
            validation::validate_url_protocol("success_url", url, true)?;
        }
    }
    if let Some(ref dc) = req.donation_config {
        if let Some(ref mission) = dc.mission {
            validation::validate_length("mission", mission, 2000)?;
        }
        if let Some(ref ty) = dc.thank_you {
            validation::validate_length("thank_you", ty, 2000)?;
        }
        if let Some(ref cn) = dc.campaign_name {
            validation::validate_length("campaign_name", cn, 200)?;
        }
        if let Some(ref url) = dc.cover_image_url {
            validation::validate_length("cover_image_url", url, 2000)?;
            if !url.is_empty() {
                validation::validate_url_protocol("cover_image_url", url, false)?;
            }
        }
        if let Some(ref pos) = dc.cover_image_position {
            validation::validate_image_position("cover_image_position", pos)?;
        }
        if let Some(ref email) = dc.contact_email {
            validation::validate_length("contact_email", email, 200)?;
        }
        if let Some(ref url) = dc.website_url {
            validation::validate_length("website_url", url, 2000)?;
            if !url.is_empty() {
                validation::validate_url_protocol("website_url", url, true)?;
            }
        }
        if let Some(ref text) = dc.social_share_text {
            validation::validate_length("social_share_text", text, 500)?;
        }
    }
    Ok(())
}
