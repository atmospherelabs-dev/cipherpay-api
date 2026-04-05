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
        &rates,
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
            let received_zec = invoices::zatoshis_to_zec(inv.received_zatoshis);
            let overpaid = inv.received_zatoshis > inv.price_zatoshis + 1000 && inv.price_zatoshis > 0;

            let merchant_origin = get_merchant_webhook_origin(pool.get_ref(), &inv.merchant_id).await;

            let is_event = if let Some(ref pid) = inv.product_id {
                crate::events::is_product_backed_by_event(pool.get_ref(), pid).await.unwrap_or(false)
            } else {
                false
            };

            let is_luma = if let Some(ref pid) = inv.product_id {
                sqlx::query_scalar::<_, Option<String>>(
                    "SELECT luma_event_id FROM events WHERE product_id = ? AND luma_event_id IS NOT NULL LIMIT 1"
                )
                .bind(pid)
                .fetch_optional(pool.get_ref())
                .await
                .ok()
                .flatten()
                .is_some()
            } else {
                false
            };

            HttpResponse::Ok().json(serde_json::json!({
                "id": inv.id,
                "memo_code": inv.memo_code,
                "product_name": inv.product_name,
                "size": inv.size,
                "amount": inv.amount,
                "price_id": inv.price_id,
                "price_eur": inv.price_eur,
                "price_usd": inv.price_usd,
                "currency": inv.currency,
                "price_zec": inv.price_zec,
                "zec_rate_at_creation": inv.zec_rate_at_creation,
                "payment_address": inv.payment_address,
                "zcash_uri": inv.zcash_uri,
                "merchant_name": inv.merchant_name,
                "merchant_origin": merchant_origin,
                "status": inv.status,
                "detected_txid": inv.detected_txid,
                "detected_at": inv.detected_at,
                "confirmed_at": inv.confirmed_at,
                "refunded_at": inv.refunded_at,
                "refund_txid": inv.refund_txid,
                "expires_at": inv.expires_at,
                "created_at": inv.created_at,
                "received_zec": received_zec,
                "price_zatoshis": inv.price_zatoshis,
                "received_zatoshis": inv.received_zatoshis,
                "overpaid": overpaid,
                "is_event": is_event,
                "is_luma": is_luma,
                "is_donation": inv.is_donation == 1,
                "payment_link_id": inv.payment_link_id,
            }))
        }
        None => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invoice not found"
        })),
    }
}

/// Extract the origin (scheme+host+port) from a merchant's webhook URL.
async fn get_merchant_webhook_origin(pool: &SqlitePool, merchant_id: &str) -> Option<String> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT webhook_url FROM merchants WHERE id = ?"
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await
    .ok()?;

    let webhook_url = row?.0?;
    url::Url::parse(&webhook_url).ok().map(|u| u.origin().ascii_serialization())
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

/// Public endpoint: finalize a draft or re-finalize an expired invoice.
/// Locks the ZEC exchange rate and starts the 15-minute payment window.
pub async fn finalize(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    price_service: web::Data<PriceService>,
    path: web::Path<String>,
) -> HttpResponse {
    let invoice_id = path.into_inner();

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch ZEC rate for finalization");
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Price feed unavailable. Please try again shortly."
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

    match invoices::finalize_invoice(
        pool.get_ref(),
        &invoice_id,
        &rates,
        fee_config.as_ref(),
    ).await {
        Ok(inv) => {
            let received_zec = invoices::zatoshis_to_zec(inv.received_zatoshis);
            let overpaid = inv.received_zatoshis > inv.price_zatoshis + 1000 && inv.price_zatoshis > 0;

            HttpResponse::Ok().json(serde_json::json!({
                "id": inv.id,
                "memo_code": inv.memo_code,
                "product_name": inv.product_name,
                "amount": inv.amount,
                "currency": inv.currency,
                "price_eur": inv.price_eur,
                "price_usd": inv.price_usd,
                "price_zec": inv.price_zec,
                "zec_rate_at_creation": inv.zec_rate_at_creation,
                "payment_address": inv.payment_address,
                "zcash_uri": inv.zcash_uri,
                "status": inv.status,
                "expires_at": inv.expires_at,
                "created_at": inv.created_at,
                "price_zatoshis": inv.price_zatoshis,
                "received_zatoshis": inv.received_zatoshis,
                "received_zec": received_zec,
                "overpaid": overpaid,
            }))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                HttpResponse::NotFound().json(serde_json::json!({ "error": msg }))
            } else if msg.contains("draft or expired") || msg.contains("already detected") || msg.contains("period has ended") {
                HttpResponse::Conflict().json(serde_json::json!({ "error": msg }))
            } else {
                tracing::error!(error = %e, "Failed to finalize invoice");
                HttpResponse::InternalServerError().json(serde_json::json!({ "error": "Failed to finalize invoice" }))
            }
        }
    }
}

fn validate_invoice_request(req: &CreateInvoiceRequest) -> Result<(), validation::ValidationError> {
    validation::validate_optional_length("product_id", &req.product_id, 100)?;
    validation::validate_optional_length("price_id", &req.price_id, 100)?;
    validation::validate_optional_length("product_name", &req.product_name, 200)?;
    validation::validate_optional_length("size", &req.size, 100)?;
    validation::validate_optional_length("currency", &req.currency, 10)?;
    if let Some(ref addr) = req.refund_address {
        if !addr.is_empty() {
            validation::validate_zcash_address("refund_address", addr)?;
        }
    }
    if req.amount < 0.0 {
        return Err(validation::ValidationError::invalid("amount", "must be non-negative"));
    }
    Ok(())
}
