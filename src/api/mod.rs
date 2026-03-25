pub mod admin;
pub mod auth;
pub mod events;
pub mod invoices;
pub mod luma;
pub mod merchants;
pub mod prices;
pub mod products;
pub mod rates;
pub mod status;
pub mod subscriptions;
pub mod tickets;
pub mod x402;

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::web;
use actix_web_lab::sse;
use base64::Engine;
use sqlx::SqlitePool;
use std::time::Duration;
use tokio::time::interval;

pub fn configure(cfg: &mut web::ServiceConfig) {
    let auth_rate_limit = GovernorConfigBuilder::default()
        .seconds_per_request(10)
        .burst_size(5)
        .finish()
        .expect("Failed to build auth rate limiter");

    cfg.service(
        web::scope("/api")
            .route("/health", web::get().to(health))
            .service(
                web::scope("/merchants")
                    .route("", web::post().to(merchants::create))
                    .route("/me", web::get().to(auth::me))
                    .route("/me", web::patch().to(auth::update_me))
                    .route("/me/invoices", web::get().to(auth::my_invoices))
                    .route("/me/regenerate-api-key", web::post().to(auth::regenerate_api_key))
                    .route("/me/regenerate-dashboard-token", web::post().to(auth::regenerate_dashboard_token))
                    .route("/me/regenerate-webhook-secret", web::post().to(auth::regenerate_webhook_secret))
                    .route("/me/billing", web::get().to(billing_summary))
                    .route("/me/billing/history", web::get().to(billing_history))
                    .route("/me/billing/settle", web::post().to(billing_settle))
                    .route("/me/delete", web::post().to(delete_account))
                    .route("/me/webhooks", web::get().to(auth::my_webhooks))
                    .route("/me/x402/history", web::get().to(x402::history))
            )
            .service(
                web::scope("/auth")
                    .wrap(Governor::new(&auth_rate_limit))
                    .route("/session", web::post().to(auth::create_session))
                    .route("/logout", web::post().to(auth::logout))
                    .route("/recover", web::post().to(auth::recover))
                    .route("/recover/confirm", web::post().to(auth::recover_confirm))
            )
            // Product endpoints (dashboard auth)
            .route("/products", web::post().to(products::create))
            .route("/products", web::get().to(products::list))
            .route("/products/{id}", web::patch().to(products::update))
            .route("/products/{id}", web::delete().to(products::deactivate))
            .route("/products/{id}/public", web::get().to(products::get_public))
            // Events endpoints (dashboard auth)
            .route("/events", web::get().to(events::list))
            .route("/events", web::post().to(events::create))
            .route("/events/{id}", web::get().to(events::get))
            .route("/events/{id}", web::patch().to(events::update))
            .route("/events/{id}/archive", web::post().to(events::archive))
            // Price endpoints
            .route("/prices", web::post().to(prices::create))
            .route("/prices/{id}", web::patch().to(prices::update))
            .route("/prices/{id}", web::delete().to(prices::deactivate))
            .route("/prices/{id}/public", web::get().to(prices::get_public))
            .route("/products/{id}/prices", web::get().to(prices::list))
            // Subscription endpoints
            .route("/subscriptions", web::post().to(subscriptions::create))
            .route("/subscriptions", web::get().to(subscriptions::list))
            .route("/subscriptions/{id}/cancel", web::post().to(subscriptions::cancel))
            // Buyer checkout (public)
            .route("/checkout", web::post().to(checkout))
            // Invoice endpoints (API key auth)
            .route("/invoices", web::post().to(invoices::create))
            .route("/invoices", web::get().to(list_invoices))
            .route("/invoices/lookup/{memo_code}", web::get().to(lookup_by_memo))
            .route("/invoices/{id}", web::get().to(invoices::get))
            .route("/invoices/{id}/status", web::get().to(status::get))
            .route("/invoices/{id}/stream", web::get().to(invoice_stream))
            .route("/invoices/{id}/finalize", web::post().to(invoices::finalize))
            .route("/invoices/{id}/cancel", web::post().to(cancel_invoice))
            .route("/invoices/{id}/refund", web::post().to(refund_invoice))
            .route("/invoices/{id}/refund-address", web::patch().to(update_refund_address))
            .route("/invoices/{id}/qr", web::get().to(qr_code))
            // Ticket endpoints
            .route("/tickets/invoice/{invoice_id}", web::get().to(tickets::by_invoice))
            .route("/tickets/scan", web::post().to(tickets::scan))
            .route("/tickets", web::get().to(tickets::list))
            .route("/tickets/{id}/void", web::post().to(tickets::void))
            .route("/rates", web::get().to(rates::get))
            // Luma integration
            .route("/luma/events", web::get().to(luma::list_events))
            .route("/luma/import", web::post().to(luma::import_event))
            .route("/luma/sync/{event_id}", web::post().to(luma::sync_event))
            .route("/invoices/{id}/luma-pass", web::get().to(luma::luma_pass))
            // x402 facilitator
            .route("/x402/verify", web::post().to(x402::verify))
            // Admin endpoints (protected by ADMIN_KEY)
            .route("/admin/auth", web::post().to(admin::auth_check))
            .route("/admin/stats", web::get().to(admin::stats))
            .route("/admin/merchants", web::get().to(admin::merchants))
            .route("/admin/billing", web::get().to(admin::billing))
            .route("/admin/webhooks", web::get().to(admin::webhooks))
            .route("/admin/system", web::get().to(admin::system)),
    );
}

/// Public checkout endpoint for buyer-driven invoice creation.
/// Accepts either `product_id` (uses default price) or `price_id` (specific price).
async fn checkout(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
    body: web::Json<CheckoutRequest>,
) -> actix_web::HttpResponse {
    if let Err(e) = validate_checkout(&body) {
        return actix_web::HttpResponse::BadRequest().json(e.to_json());
    }

    // Resolve product + pricing: either via price_id or product_id
    let (product, checkout_amount, checkout_currency, resolved_price_id, resolved_price_label, resolved_max_qty) = if let Some(ref price_id) = body.price_id {
        let price = match crate::prices::get_price(pool.get_ref(), price_id).await {
            Ok(Some(p)) if p.active == 1 => p,
            Ok(Some(_)) => {
                return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Price is no longer active"
                }));
            }
            _ => {
                return actix_web::HttpResponse::NotFound().json(serde_json::json!({
                    "error": "Price not found"
                }));
            }
        };
        let product = match crate::products::get_product(pool.get_ref(), &price.product_id).await {
            Ok(Some(p)) if p.active == 1 => p,
            _ => {
                return actix_web::HttpResponse::NotFound().json(serde_json::json!({
                    "error": "Product not found or inactive"
                }));
            }
        };
        let mq = price.max_quantity;
        (product, price.unit_amount, price.currency.clone(), Some(price.id), price.label, mq)
    } else if let Some(ref product_id) = body.product_id {
        let product = match crate::products::get_product(pool.get_ref(), product_id).await {
            Ok(Some(p)) if p.active == 1 => p,
            Ok(Some(_)) => {
                return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Product is no longer available"
                }));
            }
            _ => {
                return actix_web::HttpResponse::NotFound().json(serde_json::json!({
                    "error": "Product not found"
                }));
            }
        };
        let default_price_id = match product.default_price_id.as_ref() {
            Some(id) => id,
            None => {
                return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Product has no default price"
                }));
            }
        };
        let price = match crate::prices::get_price(pool.get_ref(), default_price_id).await {
            Ok(Some(p)) if p.active == 1 => p,
            Ok(Some(_)) => {
                return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Product default price is no longer active"
                }));
            }
            _ => {
                return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Product default price not found"
                }));
            }
        };
        let mq = price.max_quantity;
        (product, price.unit_amount, price.currency.clone(), Some(price.id), price.label, mq)
    } else {
        return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": "product_id or price_id is required"
        }));
    };

    // Resolve Luma status early (needed for capacity check and attendee validation)
    let is_luma_event: bool = sqlx::query_scalar::<_, Option<String>>(
        "SELECT luma_event_id FROM events WHERE product_id = ? AND luma_event_id IS NOT NULL",
    )
    .bind(&product.id)
    .fetch_optional(pool.get_ref())
    .await
    .ok()
    .flatten()
    .flatten()
    .is_some();

    // Enforce max_quantity: reject checkout if this tier is sold out
    if let (Some(max_qty), Some(ref pid)) = (resolved_max_qty, &resolved_price_id) {
        let sold: i64 = if is_luma_event {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM invoices WHERE price_id = ? AND status NOT IN ('expired', 'refunded')"
            )
            .bind(pid)
            .fetch_one(pool.get_ref())
            .await
            .unwrap_or(0)
        } else {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM tickets WHERE price_id = ? AND status != 'void'"
            )
            .bind(pid)
            .fetch_one(pool.get_ref())
            .await
            .unwrap_or(0)
        };

        if sold >= max_qty {
            return actix_web::HttpResponse::Conflict().json(serde_json::json!({
                "error": "Sold out",
                "detail": "This ticket tier has reached its maximum capacity"
            }));
        }
    }

    // Real-time event expiry check: block ticket sales for past events
    let event_row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, event_date FROM events WHERE product_id = ? LIMIT 1"
    )
    .bind(&product.id)
    .fetch_optional(pool.get_ref())
    .await
    .unwrap_or(None);

    if let Some((event_status, event_date)) = event_row {
        if event_status == "cancelled" || event_status == "past" {
            return actix_web::HttpResponse::Gone().json(serde_json::json!({
                "error": "Event is no longer available"
            }));
        }
        if let Some(ref date_str) = event_date {
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(date_str, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(date_str, "%Y-%m-%dT%H:%M"))
            {
                let event_utc = dt.and_utc();
                if event_utc < chrono::Utc::now() {
                    let pool_bg = pool.clone();
                    let pid = product.id.clone();
                    tokio::spawn(async move {
                        let _ = sqlx::query("UPDATE events SET status = 'past' WHERE product_id = ? AND status = 'active'")
                            .bind(&pid).execute(pool_bg.get_ref()).await;
                        let _ = sqlx::query("UPDATE products SET active = 0 WHERE id = ? AND active = 1")
                            .bind(&pid).execute(pool_bg.get_ref()).await;
                    });
                    return actix_web::HttpResponse::Gone().json(serde_json::json!({
                        "error": "Event has ended"
                    }));
                }
            }
        }
    }

    if is_luma_event {
        match &body.attendee_email {
            Some(email) if !email.is_empty() => {
                if !email.contains('@') || email.len() > 254 {
                    return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                        "error": "Valid email is required for Luma event registration"
                    }));
                }
            }
            _ => {
                return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "attendee_email is required for Luma event registration"
                }));
            }
        }
        if let Some(ref name) = body.attendee_name {
            if name.len() > 200 {
                return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "attendee_name must be 200 characters or fewer"
                }));
            }
        }
    }

    // variant field is accepted for backward compatibility but no longer validated
    let _ = &body.variant;

    let merchant = match crate::merchants::get_merchant_by_id(pool.get_ref(), &product.merchant_id, &config.encryption_key).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Merchant not found"
            }));
        }
        Err(_) => {
            return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };

    if config.fee_enabled() {
        if let Ok(status) = crate::billing::get_merchant_billing_status(pool.get_ref(), &merchant.id).await {
            if status == "past_due" || status == "suspended" {
                return actix_web::HttpResponse::PaymentRequired().json(serde_json::json!({
                    "error": "Merchant account has outstanding fees",
                    "billing_status": status,
                }));
            }
        }
    }

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch ZEC rate for checkout");
            return actix_web::HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Price feed unavailable"
            }));
        }
    };

    let invoice_req = crate::invoices::CreateInvoiceRequest {
        product_id: Some(product.id.clone()),
        price_id: resolved_price_id,
        product_name: Some(product.name.clone()),
        size: body.variant.clone(),
        amount: checkout_amount,
        currency: Some(checkout_currency),
        refund_address: body.refund_address.clone(),
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
            // Store encrypted attendee PII for Luma events
            if is_luma_event {
                let enc_key = &config.encryption_key;
                let enc_name = body.attendee_name.as_deref()
                    .filter(|n| !n.is_empty())
                    .map(|n| if enc_key.is_empty() { Ok(n.to_string()) } else { crate::crypto::encrypt(n, enc_key) })
                    .transpose()
                    .ok()
                    .flatten();
                let enc_email = body.attendee_email.as_deref()
                    .filter(|e| !e.is_empty())
                    .map(|e| if enc_key.is_empty() { Ok(e.to_string()) } else { crate::crypto::encrypt(e, enc_key) })
                    .transpose()
                    .ok()
                    .flatten();

                sqlx::query(
                    "UPDATE invoices SET attendee_name = ?, attendee_email = ?, luma_registration_status = 'pending' WHERE id = ?",
                )
                .bind(&enc_name)
                .bind(&enc_email)
                .bind(&resp.invoice_id)
                .execute(pool.get_ref())
                .await
                .ok();
            }

            let mut payload = serde_json::to_value(&resp).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("price_label".to_string(), serde_json::to_value(resolved_price_label).unwrap_or(serde_json::Value::Null));
                if let Ok(Some(ctx)) = crate::events::get_event_context_by_product(pool.get_ref(), &product.id).await {
                    obj.insert("event_title".to_string(), serde_json::Value::String(ctx.event_title));
                    obj.insert("event_date".to_string(), serde_json::to_value(ctx.event_date).unwrap_or(serde_json::Value::Null));
                    obj.insert("event_location".to_string(), serde_json::to_value(ctx.event_location).unwrap_or(serde_json::Value::Null));
                }
                obj.insert("is_luma".to_string(), serde_json::json!(is_luma_event));
            }
            actix_web::HttpResponse::Created().json(payload)
        }
        Err(e) => {
            tracing::error!(error = %e, "Checkout invoice creation failed");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create invoice"
            }))
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct CheckoutRequest {
    product_id: Option<String>,
    price_id: Option<String>,
    variant: Option<String>,
    refund_address: Option<String>,
    attendee_name: Option<String>,
    attendee_email: Option<String>,
}

fn validate_checkout(req: &CheckoutRequest) -> Result<(), crate::validation::ValidationError> {
    if req.product_id.is_none() && req.price_id.is_none() {
        return Err(crate::validation::ValidationError::invalid(
            "product_id", "either product_id or price_id is required"
        ));
    }
    if let Some(ref pid) = req.product_id {
        crate::validation::validate_length("product_id", pid, 100)?;
    }
    if let Some(ref pid) = req.price_id {
        crate::validation::validate_length("price_id", pid, 100)?;
    }
    crate::validation::validate_optional_length("variant", &req.variant, 100)?;
    if let Some(ref addr) = req.refund_address {
        if !addr.is_empty() {
            crate::validation::validate_zcash_address("refund_address", addr)?;
        }
    }
    Ok(())
}

async fn health() -> actix_web::HttpResponse {
    actix_web::HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "service": "cipherpay",
    }))
}

/// List invoices: requires API key or session auth. Scoped to the authenticated merchant.
async fn list_invoices(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    let merchant = match auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            if let Some(auth_header) = req.headers().get("Authorization") {
                if let Ok(auth_str) = auth_header.to_str() {
                    let key = auth_str.strip_prefix("Bearer ").unwrap_or(auth_str).trim();
                    let enc_key = req.app_data::<web::Data<crate::config::Config>>()
                        .map(|c| c.encryption_key.clone()).unwrap_or_default();
                    match crate::merchants::authenticate(&pool, key, &enc_key).await {
                        Ok(Some(m)) => m,
                        _ => return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({"error": "Invalid API key"})),
                    }
                } else {
                    return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({"error": "Not authenticated"}));
                }
            } else {
                return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({"error": "Not authenticated"}));
            }
        }
    };

    let rows = sqlx::query(
        "SELECT id, merchant_id, memo_code, product_id, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
         amount, price_id,
         payment_address, zcash_uri,
         status, detected_txid,
         detected_at, expires_at, confirmed_at, refunded_at,
         refund_address, created_at, price_zatoshis, received_zatoshis,
         (EXISTS (SELECT 1 FROM events WHERE product_id = invoices.product_id)) AS is_event
         FROM invoices WHERE merchant_id = ? ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(rows) => {
            use sqlx::Row;
            let invoices: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    let pz = r.get::<i64, _>("price_zatoshis");
                    let rz = r.get::<i64, _>("received_zatoshis");
                    serde_json::json!({
                        "id": r.get::<String, _>("id"),
                        "merchant_id": r.get::<String, _>("merchant_id"),
                        "memo_code": r.get::<String, _>("memo_code"),
                        "product_id": r.get::<Option<String>, _>("product_id"),
                        "product_name": r.get::<Option<String>, _>("product_name"),
                        "size": r.get::<Option<String>, _>("size"),
                        "price_eur": r.get::<f64, _>("price_eur"),
                        "price_usd": r.get::<Option<f64>, _>("price_usd"),
                        "currency": r.get::<Option<String>, _>("currency"),
                        "price_zec": r.get::<f64, _>("price_zec"),
                        "zec_rate": r.get::<f64, _>("zec_rate_at_creation"),
                        "amount": r.get::<Option<f64>, _>("amount"),
                        "price_id": r.get::<Option<String>, _>("price_id"),
                        "payment_address": r.get::<String, _>("payment_address"),
                        "zcash_uri": r.get::<String, _>("zcash_uri"),
                        "status": r.get::<String, _>("status"),
                        "detected_txid": r.get::<Option<String>, _>("detected_txid"),
                        "detected_at": r.get::<Option<String>, _>("detected_at"),
                        "expires_at": r.get::<String, _>("expires_at"),
                        "confirmed_at": r.get::<Option<String>, _>("confirmed_at"),
                        "refunded_at": r.get::<Option<String>, _>("refunded_at"),
                        "refund_address": r.get::<Option<String>, _>("refund_address"),
                        "created_at": r.get::<String, _>("created_at"),
                        "received_zec": crate::invoices::zatoshis_to_zec(rz),
                        "price_zatoshis": pz,
                        "received_zatoshis": rz,
                        "overpaid": rz > pz + 1000 && pz > 0,
                        "is_event": r.get::<bool, _>("is_event"),
                    })
                })
                .collect();
            actix_web::HttpResponse::Ok().json(invoices)
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list invoices");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

async fn lookup_by_memo(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let memo_code = path.into_inner();

    match crate::invoices::get_invoice_by_memo(pool.get_ref(), &memo_code).await {
        Ok(Some(inv)) => {
            let received_zec = crate::invoices::zatoshis_to_zec(inv.received_zatoshis);
            let overpaid = inv.received_zatoshis > inv.price_zatoshis + 1000 && inv.price_zatoshis > 0;
            actix_web::HttpResponse::Ok().json(serde_json::json!({
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
                "status": inv.status,
                "detected_txid": inv.detected_txid,
                "detected_at": inv.detected_at,
                "confirmed_at": inv.confirmed_at,
                "refunded_at": inv.refunded_at,
                "expires_at": inv.expires_at,
                "created_at": inv.created_at,
                "received_zec": received_zec,
                "price_zatoshis": inv.price_zatoshis,
                "received_zatoshis": inv.received_zatoshis,
                "overpaid": overpaid,
            }))
        },
        Ok(None) => actix_web::HttpResponse::NotFound().json(serde_json::json!({
            "error": "No invoice found for this memo code"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to lookup invoice by memo");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

/// SSE stream for invoice status updates -- replaces client-side polling.
/// The server polls the DB internally and pushes only when state changes.
async fn invoice_stream(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> impl actix_web::Responder {
    let invoice_id = path.into_inner();
    let (tx, rx) = tokio::sync::mpsc::channel::<sse::Event>(10);

    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(2));
        let mut last_status = String::new();

        // Send initial state immediately
        if let Ok(Some(status)) = crate::invoices::get_invoice_status(&pool, &invoice_id).await {
            last_status.clone_from(&status.status);
            let data = serde_json::json!({
                "status": status.status,
                "txid": status.detected_txid,
                "received_zatoshis": status.received_zatoshis,
                "price_zatoshis": status.price_zatoshis,
            });
            let _ = tx
                .send(sse::Data::new(data.to_string()).event("status").into())
                .await;
        }

        let mut last_received: i64 = 0;
        loop {
            tick.tick().await;

            match crate::invoices::get_invoice_status(&pool, &invoice_id).await {
                Ok(Some(status)) => {
                    let amounts_changed = status.received_zatoshis != last_received;
                    if status.status != last_status || amounts_changed {
                        last_status.clone_from(&status.status);
                        last_received = status.received_zatoshis;
                        let data = serde_json::json!({
                            "status": status.status,
                            "txid": status.detected_txid,
                            "received_zatoshis": status.received_zatoshis,
                            "price_zatoshis": status.price_zatoshis,
                        });
                        if tx
                            .send(sse::Data::new(data.to_string()).event("status").into())
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if status.status == "confirmed" || status.status == "expired" {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    });

    sse::Sse::from_infallible_receiver(rx).with_retry_duration(Duration::from_secs(5))
}

/// Generate a QR code PNG for a zcash: payment URI (ZIP-321 compliant)
async fn qr_code(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let invoice_id = path.into_inner();

    let invoice = match crate::invoices::get_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(inv)) => inv,
        _ => return actix_web::HttpResponse::NotFound().finish(),
    };

    let uri = if invoice.zcash_uri.is_empty() {
        let memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(invoice.memo_code.as_bytes());
        format!("zcash:{}?amount={:.8}&memo={}", invoice.payment_address, invoice.price_zec, memo_b64)
    } else {
        invoice.zcash_uri.clone()
    };

    match generate_qr_png(&uri) {
        Ok(png_bytes) => actix_web::HttpResponse::Ok()
            .content_type("image/png")
            .body(png_bytes),
        Err(_) => actix_web::HttpResponse::InternalServerError().finish(),
    }
}

fn generate_qr_png(data: &str) -> anyhow::Result<Vec<u8>> {
    use image::Luma;
    use qrcode::QrCode;

    let code = QrCode::new(data.as_bytes())?;
    let img = code
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .min_dimensions(250, 250)
        .build();

    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)?;
    Ok(buf.into_inner())
}

/// Cancel a pending invoice (only pending invoices can be cancelled)
async fn cancel_invoice(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let merchant = match auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let invoice_id = path.into_inner();

    match crate::invoices::get_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(inv)) if inv.merchant_id == merchant.id && inv.status == "pending" => {
            if inv.product_name.as_deref() == Some("Fee Settlement") {
                return actix_web::HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Settlement invoices cannot be cancelled"
                }));
            }
            if let Err(e) = crate::invoices::mark_expired(pool.get_ref(), &invoice_id).await {
                tracing::error!(error = %e, invoice_id = %invoice_id, "Failed to cancel invoice");
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to cancel invoice"
                }));
            }
            actix_web::HttpResponse::Ok().json(serde_json::json!({ "status": "cancelled" }))
        }
        Ok(Some(_)) => {
            actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Only pending invoices can be cancelled"
            }))
        }
        _ => {
            actix_web::HttpResponse::NotFound().json(serde_json::json!({
                "error": "Invoice not found"
            }))
        }
    }
}

/// Mark an invoice as refunded (dashboard auth)
async fn refund_invoice(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
    body: web::Json<serde_json::Value>,
) -> actix_web::HttpResponse {
    let merchant = match auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let invoice_id = path.into_inner();
    let refund_txid = body.get("refund_txid").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

    match crate::invoices::get_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(inv)) if inv.merchant_id == merchant.id && inv.status == "confirmed" => {
            if let Err(e) = crate::invoices::mark_refunded(pool.get_ref(), &invoice_id, refund_txid).await {
                tracing::error!(error = %e, invoice_id = %invoice_id, "Failed to mark invoice refunded");
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to process refund"
                }));
            }
            let response = serde_json::json!({
                "status": "refunded",
                "refund_address": inv.refund_address,
                "refund_txid": refund_txid,
            });
            actix_web::HttpResponse::Ok().json(response)
        }
        Ok(Some(_)) => {
            actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Only confirmed invoices can be refunded"
            }))
        }
        _ => {
            actix_web::HttpResponse::NotFound().json(serde_json::json!({
                "error": "Invoice not found"
            }))
        }
    }
}

/// Buyer can save a refund address on their invoice (write-once).
/// No auth required: invoice IDs are unguessable UUIDs and the address
/// can only be set once (write-once guard in update_refund_address).
async fn update_refund_address(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
    body: web::Json<serde_json::Value>,
) -> actix_web::HttpResponse {
    let invoice_id = path.into_inner();

    let address = match body.get("refund_address").and_then(|v| v.as_str()) {
        Some(a) if !a.is_empty() => a,
        _ => {
            return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                "error": "refund_address is required"
            }));
        }
    };

    if let Err(e) = crate::validation::validate_zcash_address("refund_address", address) {
        return actix_web::HttpResponse::BadRequest().json(e.to_json());
    }

    match crate::invoices::update_refund_address(pool.get_ref(), &invoice_id, address).await {
        Ok(true) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "status": "saved",
            "refund_address": address,
        })),
        Ok(false) => actix_web::HttpResponse::Conflict().json(serde_json::json!({
            "error": "Refund address is already set or invoice status does not allow changes"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update refund address");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

async fn billing_summary(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
) -> actix_web::HttpResponse {
    let merchant = match auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    if !config.fee_enabled() {
        return actix_web::HttpResponse::Ok().json(serde_json::json!({
            "fee_enabled": false,
            "fee_rate": 0.0,
            "billing_status": "active",
            "trust_tier": "standard",
        }));
    }

    match crate::billing::get_billing_summary(pool.get_ref(), &merchant.id, &config).await {
        Ok(summary) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "fee_enabled": true,
            "fee_rate": summary.fee_rate,
            "trust_tier": summary.trust_tier,
            "billing_status": summary.billing_status,
            "current_cycle": summary.current_cycle,
            "total_fees_zec": summary.total_fees_zec,
            "auto_collected_zec": summary.auto_collected_zec,
            "outstanding_zec": summary.outstanding_zec,
            "min_settlement_zec": 0.05,
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get billing summary");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

async fn billing_history(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    let merchant = match auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    match crate::billing::get_billing_history(pool.get_ref(), &merchant.id).await {
        Ok(cycles) => actix_web::HttpResponse::Ok().json(cycles),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get billing history");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

async fn billing_settle(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
) -> actix_web::HttpResponse {
    let merchant = match auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let fee_address = match &config.fee_address {
        Some(addr) => addr.clone(),
        None => {
            return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Billing not enabled"
            }));
        }
    };

    let summary = match crate::billing::get_billing_summary(pool.get_ref(), &merchant.id, &config).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get billing for settle");
            return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };

    if summary.outstanding_zec < 0.00001 {
        return actix_web::HttpResponse::Ok().json(serde_json::json!({
            "message": "No outstanding balance",
            "outstanding_zec": 0.0,
        }));
    }

    const MIN_SETTLEMENT_ZEC: f64 = 0.05;
    if summary.outstanding_zec < MIN_SETTLEMENT_ZEC {
        return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Outstanding balance ({:.6} ZEC) is below the minimum settlement amount ({:.2} ZEC). Fees will carry over until the threshold is reached.", summary.outstanding_zec, MIN_SETTLEMENT_ZEC),
            "outstanding_zec": summary.outstanding_zec,
            "min_settlement_zec": MIN_SETTLEMENT_ZEC,
        }));
    }

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(_) => crate::invoices::pricing::ZecRates {
            zec_eur: 0.0, zec_usd: 0.0, zec_brl: 0.0,
            zec_gbp: 0.0, zec_cad: 0.0, zec_jpy: 0.0,
            zec_mxn: 0.0, zec_ars: 0.0, zec_ngn: 0.0,
            zec_chf: 0.0, zec_inr: 0.0,
            updated_at: chrono::Utc::now(),
        },
    };

    match crate::billing::create_settlement_invoice(
        pool.get_ref(), &merchant.id, summary.outstanding_zec, &fee_address, rates.zec_eur, rates.zec_usd,
    ).await {
        Ok(invoice_id) => {
            if let Some(cycle) = &summary.current_cycle {
                let _ = sqlx::query(
                    "UPDATE billing_cycles SET settlement_invoice_id = ?, status = 'invoiced',
                     grace_until = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '+7 days')
                     WHERE id = ? AND status = 'open'"
                )
                .bind(&invoice_id)
                .bind(&cycle.id)
                .execute(pool.get_ref())
                .await;
            }

            actix_web::HttpResponse::Created().json(serde_json::json!({
                "invoice_id": invoice_id,
                "outstanding_zec": summary.outstanding_zec,
                "message": "Settlement invoice created. Pay to restore full access.",
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to create settlement invoice");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create settlement invoice"
            }))
        }
    }
}

async fn delete_account(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
) -> actix_web::HttpResponse {
    let merchant = match auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return actix_web::HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    if config.fee_enabled() {
        match crate::merchants::has_outstanding_balance(pool.get_ref(), &merchant.id).await {
            Ok(true) => {
                return actix_web::HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Cannot delete account with outstanding billing balance. Please settle your fees first."
                }));
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to check billing balance");
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }));
            }
            _ => {}
        }
    }

    match crate::merchants::delete_merchant(pool.get_ref(), &merchant.id).await {
        Ok(()) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "status": "deleted",
            "message": "Your account and all associated data have been permanently deleted."
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete merchant account");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to delete account"
            }))
        }
    }
}
