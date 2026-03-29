use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::payment_links::{self, CreatePaymentLinkRequest, UpdatePaymentLinkRequest};
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
            let result: Vec<_> = links.iter().map(link_response).collect();
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

/// Public endpoint: resolve a payment link by slug and create an invoice.
/// Rate limited to prevent invoice flooding.
pub async fn resolve(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
    path: web::Path<String>,
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

    let price = match crate::prices::get_price(pool.get_ref(), &link.price_id).await {
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
            tracing::error!(error = %e, slug = %slug, "Payment link invoice creation failed");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create invoice"
            }))
        }
    }
}

fn link_response(link: &payment_links::PaymentLink) -> serde_json::Value {
    serde_json::json!({
        "id": link.id,
        "merchant_id": link.merchant_id,
        "price_id": link.price_id,
        "slug": link.slug,
        "name": link.name,
        "success_url": link.success_url,
        "metadata": link.metadata_json(),
        "active": link.active == 1,
        "total_created": link.total_created,
        "created_at": link.created_at,
    })
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

fn validate_update(req: &UpdatePaymentLinkRequest) -> Result<(), validation::ValidationError> {
    if let Some(ref name) = req.name {
        validation::validate_length("name", name, 200)?;
    }
    if let Some(ref url) = req.success_url {
        validation::validate_length("success_url", url, 2000)?;
    }
    Ok(())
}
