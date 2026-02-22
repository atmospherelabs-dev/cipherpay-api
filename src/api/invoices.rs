use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::invoices::{self, CreateInvoiceRequest};
use crate::invoices::pricing::PriceService;
use crate::validation;

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    price_service: web::Data<PriceService>,
    body: web::Json<CreateInvoiceRequest>,
) -> HttpResponse {
    if let Err(e) = validate_invoice_request(&body) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    let merchant = match resolve_merchant(&req, &pool, &config).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid API key or no merchant configured. Register via POST /api/merchants first."
            }));
        }
    };

    if config.fee_enabled() {
        if let Ok(status) = crate::billing::get_merchant_billing_status(pool.get_ref(), &merchant.id).await {
            if status == "past_due" || status == "suspended" {
                return HttpResponse::PaymentRequired().json(serde_json::json!({
                    "error": "Merchant account has outstanding fees",
                    "billing_status": status,
                }));
            }
        }
    }

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch ZEC rate");
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Price feed unavailable"
            }));
        }
    };

    let fee_config = if config.fee_enabled() {
        config.fee_address.as_ref().map(|addr| invoices::FeeConfig {
            fee_address: addr.clone(),
            fee_rate: config.fee_rate,
        })
    } else {
        None
    };

    match invoices::create_invoice(
        pool.get_ref(),
        &merchant.id,
        &merchant.ufvk,
        &body,
        rates.zec_eur,
        rates.zec_usd,
        config.invoice_expiry_minutes,
        fee_config.as_ref(),
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

/// Public invoice GET: returns only checkout-safe fields.
/// Shipping info is NEVER exposed to unauthenticated callers.
pub async fn get(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let id_or_memo = path.into_inner();

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
        Some(inv) => {
            HttpResponse::Ok().json(serde_json::json!({
                "id": inv.id,
                "memo_code": inv.memo_code,
                "product_name": inv.product_name,
                "size": inv.size,
                "price_eur": inv.price_eur,
                "price_usd": inv.price_usd,
                "currency": inv.currency,
                "price_zec": inv.price_zec,
                "zec_rate_at_creation": inv.zec_rate_at_creation,
                "payment_address": inv.payment_address,
                "zcash_uri": inv.zcash_uri,
                "merchant_name": inv.merchant_name,
                "status": inv.status,
                "detected_txid": inv.detected_txid,
                "detected_at": inv.detected_at,
                "confirmed_at": inv.confirmed_at,
                "refunded_at": inv.refunded_at,
                "expires_at": inv.expires_at,
                "created_at": inv.created_at,
            }))
        }
        None => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invoice not found"
        })),
    }
}

/// Resolve the merchant from the request:
/// 1. If Authorization header has "Bearer cpay_...", authenticate by API key
/// 2. Try session cookie (dashboard)
/// 3. In testnet, fall back to sole merchant (single-tenant test mode)
async fn resolve_merchant(
    req: &HttpRequest,
    pool: &SqlitePool,
    config: &Config,
) -> Option<crate::merchants::Merchant> {
    if let Some(auth) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth.to_str() {
            let key = auth_str
                .strip_prefix("Bearer ")
                .unwrap_or(auth_str)
                .trim();

            if key.starts_with("cpay_sk_") || key.starts_with("cpay_") {
                return crate::merchants::authenticate(pool, key, &config.encryption_key)
                    .await
                    .ok()
                    .flatten();
            }
        }
    }

    if let Some(merchant) = crate::api::auth::resolve_session(req, pool).await {
        return Some(merchant);
    }

    // Single-tenant fallback: ONLY in testnet mode
    if config.is_testnet() {
        return crate::merchants::get_all_merchants(pool, &config.encryption_key)
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
            });
    }

    None
}

fn validate_invoice_request(req: &CreateInvoiceRequest) -> Result<(), validation::ValidationError> {
    validation::validate_optional_length("product_id", &req.product_id, 100)?;
    validation::validate_optional_length("product_name", &req.product_name, 200)?;
    validation::validate_optional_length("size", &req.size, 100)?;
    validation::validate_optional_length("currency", &req.currency, 10)?;
    if let Some(ref addr) = req.refund_address {
        if !addr.is_empty() {
            validation::validate_zcash_address("refund_address", addr)?;
        }
    }
    if req.price_eur < 0.0 {
        return Err(validation::ValidationError::invalid("price_eur", "must be non-negative"));
    }
    Ok(())
}
