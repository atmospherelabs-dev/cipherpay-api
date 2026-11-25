use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::invoices::pricing::PriceService;
use crate::invoices::{self, CreateInvoiceRequest};
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
        if let Ok(status) =
            crate::billing::get_merchant_billing_status(pool.get_ref(), &merchant.id).await
        {
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

    let fee_config =
        crate::billing::fee_config_for_merchant(pool.get_ref(), &merchant.id, &config).await;

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
        Ok(resp) => {
            let url=format!("{}/pay/{}#checkout={}",config.frontend_url.as_deref().unwrap_or("https://cipherpay.app"),resp.invoice_id,resp.checkout_token);
            let mut value=serde_json::to_value(resp).unwrap(); value["checkout_url"]=serde_json::json!(url);
            HttpResponse::Created().insert_header(("Cache-Control","no-store")).json(value)
        },
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
pub async fn get(pool: web::Data<SqlitePool>, path: web::Path<String>) -> HttpResponse {
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
            let overpaid =
                inv.received_zatoshis > inv.price_zatoshis + 1000 && inv.price_zatoshis > 0;

            let is_crosschain = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM crosschain_attempts WHERE invoice_id=?)",
            )
            .bind(&inv.id)
            .fetch_one(pool.get_ref())
            .await
            .unwrap_or(false);

            let merchant_origin =
                get_merchant_webhook_origin(pool.get_ref(), &inv.merchant_id).await;

            let is_event = if let Some(ref pid) = inv.product_id {
                crate::events::is_product_backed_by_event(pool.get_ref(), pid)
                    .await
                    .unwrap_or(false)
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

            let donation_meta = if inv.is_donation == 1 {
                if let Some(ref link_id) = inv.payment_link_id {
                    let row: Option<(Option<String>, String)> = sqlx::query_as(
                        "SELECT donation_config, slug FROM payment_links WHERE id = ?",
                    )
                    .bind(link_id)
                    .fetch_optional(pool.get_ref())
                    .await
                    .ok()
                    .flatten();
                    row.map(|(dc_json, slug)| {
                        let dc: Option<crate::payment_links::DonationConfig> = dc_json
                            .as_deref()
                            .and_then(|j| serde_json::from_str(j).ok());
                        serde_json::json!({
                            "thank_you": dc.as_ref().and_then(|d| d.thank_you.clone()),
                            "campaign_name": dc.as_ref().and_then(|d| d.campaign_name.clone()),
                            "contact_email": dc.as_ref().and_then(|d| d.contact_email.clone()),
                            "social_share_text": dc.as_ref().and_then(|d| d.social_share_text.clone()),
                            "slug": slug,
                        })
                    })
                } else {
                    None
                }
            } else {
                None
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
                "is_crosschain": is_crosschain,
                "is_event": is_event,
                "is_luma": is_luma,
                "is_donation": inv.is_donation == 1,
                "payment_link_id": inv.payment_link_id,
                "donation_meta": donation_meta,
            }))
        }
        None => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invoice not found"
        })),
    }
}

/// Public, machine-readable payment request for agents and wallets.
/// Looks up by invoice ID only (not memo code) and exposes no shipping data,
/// refund address or checkout capability token.
pub async fn payment_request(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    path: web::Path<String>,
) -> HttpResponse {
    use crate::invoices::payment_request as pr;
    let id = path.into_inner();
    let inv = match invoices::get_invoice(pool.get_ref(), &id).await {
        Ok(Some(inv)) => inv,
        Ok(None) => {
            return HttpResponse::NotFound().json(serde_json::json!({"error": "Invoice not found"}))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get invoice for payment request");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };
    let flags = sqlx::query_as::<_, (bool, bool)>(
        "SELECT expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now'),
                EXISTS(SELECT 1 FROM crosschain_attempts WHERE invoice_id = invoices.id)
         FROM invoices WHERE id = ?",
    )
    .bind(&inv.id)
    .fetch_one(pool.get_ref())
    .await;
    let (unexpired, crosschain_started) = match flags {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read payment request state");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let outputs = pr::outputs(
        &inv.zcash_uri,
        &inv.id,
        &inv.payment_address,
        &inv.memo_code,
        inv.price_zatoshis,
    );
    let mut reason = pr::not_payable_reason(
        &inv.status,
        unexpired,
        inv.received_zatoshis,
        inv.detected_txid.is_some(),
        crosschain_started,
    );
    if reason.is_none() && outputs.is_none() {
        reason = Some("invalid_payment_request");
    }
    let payable = reason.is_none();
    let outputs = if payable { outputs.unwrap_or_default() } else { Vec::new() };
    let total: i64 = outputs.iter().map(|o| o.amount_zatoshis).sum();
    let frontend = config.frontend_url.as_deref().unwrap_or("https://cipherpay.app");
    let merchant_origin = get_merchant_webhook_origin(pool.get_ref(), &inv.merchant_id).await;

    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .insert_header(("Access-Control-Allow-Origin", "*"))
        .json(serde_json::json!({
            "version": pr::VERSION,
            "invoice_id": inv.id,
            "network": if config.is_testnet() { "zcash:testnet" } else { "zcash:mainnet" },
            "status": inv.status,
            "payable": payable,
            "not_payable_reason": reason,
            "expires_at": inv.expires_at,
            "merchant": { "name": inv.merchant_name, "origin": merchant_origin },
            "description": inv.product_name,
            "price": { "amount": inv.amount.unwrap_or(inv.price_eur), "currency": inv.currency.as_deref().unwrap_or("EUR") },
            "price_zatoshis": inv.price_zatoshis,
            "total_zatoshis": total,
            "outputs": outputs,
            "zcash_uri": if payable { Some(inv.zcash_uri.as_str()) } else { None },
            "instructions": "Pay each required output (and optionally the platform fee output) in one shielded transaction, with the exact amount and memo. Do not pay when payable is false.",
            "status_url": format!("/api/invoices/{}/status", inv.id),
            "checkout_url": format!("{}/pay/{}", frontend, inv.id),
        }))
}

/// Extract the origin (scheme+host+port) from a merchant's webhook URL.
async fn get_merchant_webhook_origin(pool: &SqlitePool, merchant_id: &str) -> Option<String> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT webhook_url FROM merchants WHERE id = ?")
            .bind(merchant_id)
            .fetch_optional(pool)
            .await
            .ok()?;

    let webhook_url = row?.0?;
    url::Url::parse(&webhook_url)
        .ok()
        .map(|u| u.origin().ascii_serialization())
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
            let key = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str).trim();

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

    let fee_config = match invoices::get_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(inv)) => {
            crate::billing::fee_config_for_merchant(pool.get_ref(), &inv.merchant_id, &config).await
        }
        _ => None,
    };

    match invoices::finalize_invoice(pool.get_ref(), &invoice_id, &rates, fee_config.as_ref()).await
    {
        Ok(inv) => {
            let received_zec = invoices::zatoshis_to_zec(inv.received_zatoshis);
            let overpaid =
                inv.received_zatoshis > inv.price_zatoshis + 1000 && inv.price_zatoshis > 0;

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
            } else if msg.contains("draft or expired")
                || msg.contains("already detected")
                || msg.contains("period has ended")
                || msg.contains("not eligible for renewal")
            {
                HttpResponse::Conflict().json(serde_json::json!({ "error": msg }))
            } else {
                tracing::error!(error = %e, "Failed to finalize invoice");
                HttpResponse::InternalServerError()
                    .json(serde_json::json!({ "error": "Failed to finalize invoice" }))
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
        return Err(validation::ValidationError::invalid(
            "amount",
            "must be non-negative",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod payment_request_tests {
    use super::*;
    use actix_web::{test, App};
    use base64::Engine;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }

    #[actix_web::test]
    async fn payment_request_reports_outputs_and_refuses_unsafe_states() {
        let pool = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&pool, "m").await;
        crate::repair_tests::invoice(&pool, "i", "m").await;
        let uri = format!(
            "zcash:?address=u1inv&amount=0.00001000&memo={}&address.1=u1fee&amount.1=0.00000100&memo.1={}",
            b64("CP-i"),
            b64("FEE-i")
        );
        sqlx::query("UPDATE invoices SET payment_address='u1inv', zcash_uri=?, product_name='Ice', status='pending' WHERE id='i'")
            .bind(&uri).execute(&pool).await.unwrap();
        let config = crate::config::Config::from_env().unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(config))
                .route("/{id}", web::get().to(payment_request)),
        )
        .await;
        let get = |path: &'static str| test::TestRequest::get().uri(path).to_request();

        let body: serde_json::Value = test::call_and_read_body_json(&app, get("/i")).await;
        assert_eq!(body["payable"], true);
        assert_eq!(body["total_zatoshis"], 1100);
        assert_eq!(body["outputs"][0]["amount_zatoshis"], 1000);
        assert_eq!(body["outputs"][0]["memo"], "CP-i");
        assert_eq!(body["outputs"][0]["required"], true);
        assert_eq!(body["outputs"][1]["purpose"], "platform_fee");
        assert_eq!(body["outputs"][1]["required"], false);
        assert!(body.get("checkout_token").is_none());
        assert!(!body.to_string().contains("refund_address\":\""));

        // Memo codes are not accepted as lookup keys.
        assert_eq!(test::call_service(&app, get("/CP-i")).await.status(), 404);

        // A started cross-chain quote makes direct payment unsafe.
        sqlx::query("INSERT INTO crosschain_attempts (invoice_id,state) VALUES ('i','CREATING')")
            .execute(&pool).await.unwrap();
        let body: serde_json::Value = test::call_and_read_body_json(&app, get("/i")).await;
        assert_eq!(body["payable"], false);
        assert_eq!(body["not_payable_reason"], "crosschain_in_progress");
        assert_eq!(body["outputs"].as_array().unwrap().len(), 0);
        assert!(body["zcash_uri"].is_null());
        sqlx::query("DELETE FROM crosschain_attempts").execute(&pool).await.unwrap();

        sqlx::query("UPDATE invoices SET expires_at='2000-01-01T00:00:00Z' WHERE id='i'")
            .execute(&pool).await.unwrap();
        let body: serde_json::Value = test::call_and_read_body_json(&app, get("/i")).await;
        assert_eq!(body["not_payable_reason"], "expired");

        sqlx::query("UPDATE invoices SET expires_at='2099-01-01T00:00:00Z', zcash_uri='zcash:u1evil?amount=0.00001&memo=' WHERE id='i'")
            .execute(&pool).await.unwrap();
        let body: serde_json::Value = test::call_and_read_body_json(&app, get("/i")).await;
        assert_eq!(body["not_payable_reason"], "invalid_payment_request");
    }
}
