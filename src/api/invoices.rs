use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::invoices::{self, CreateInvoiceRequest};
use crate::invoices::pricing::PriceService;

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    price_service: web::Data<PriceService>,
    body: web::Json<CreateInvoiceRequest>,
) -> HttpResponse {
    let merchant = match resolve_merchant(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid API key or no merchant configured. Register via POST /api/merchants first."
            }));
        }
    };

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch ZEC rate");
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Price feed unavailable"
            }));
        }
    };

    match invoices::create_invoice(
        pool.get_ref(),
        &merchant.id,
        &merchant.payment_address,
        &body,
        rates.zec_eur,
        config.invoice_expiry_minutes,
    )
    .await
    {
        Ok(resp) => HttpResponse::Created().json(resp),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create invoice");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create invoice"
            }))
        }
    }
}

pub async fn get(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let id_or_memo = path.into_inner();

    // Try as UUID first, then as memo code
    let invoice = match invoices::get_invoice(pool.get_ref(), &id_or_memo).await {
        Ok(Some(inv)) => Some(inv),
        Ok(None) => invoices::get_invoice_by_memo(pool.get_ref(), &id_or_memo)
            .await
            .ok()
            .flatten(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get invoice");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };

    match invoice {
        Some(inv) => HttpResponse::Ok().json(inv),
        None => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invoice not found"
        })),
    }
}

/// Resolve the merchant from the request:
/// 1. If Authorization header has "Bearer cpay_...", authenticate by API key
/// 2. Otherwise fall back to the sole merchant (single-tenant / test mode)
async fn resolve_merchant(
    req: &HttpRequest,
    pool: &SqlitePool,
) -> Option<crate::merchants::Merchant> {
    // Try API key from Authorization header
    if let Some(auth) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth.to_str() {
            let key = auth_str
                .strip_prefix("Bearer ")
                .unwrap_or(auth_str)
                .trim();

            if key.starts_with("cpay_") {
                return crate::merchants::authenticate(pool, key)
                    .await
                    .ok()
                    .flatten();
            }
        }
    }

    // Fallback: single-tenant mode (test console, or self-hosted with one merchant)
    crate::merchants::get_all_merchants(pool)
        .await
        .ok()
        .and_then(|m| {
            if m.len() == 1 {
                m.into_iter().next()
            } else {
                tracing::warn!(
                    count = m.len(),
                    "Multiple merchants but no API key provided"
                );
                None
            }
        })
}
