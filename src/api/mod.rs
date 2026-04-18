pub mod admin;
pub mod auth;
pub mod billing_routes;
pub mod events;
pub mod invoice_routes;
pub mod invoices;
pub mod luma;
pub mod merchants;
pub mod payment_links;
pub mod prices;
pub mod products;
pub mod rates;
pub mod sessions;
pub mod status;
pub mod subscriptions;
pub mod system;
pub mod tickets;
pub mod x402;

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::web;
use sqlx::SqlitePool;

pub fn configure(cfg: &mut web::ServiceConfig) {
    let auth_rate_limit = GovernorConfigBuilder::default()
        .seconds_per_request(10)
        .burst_size(5)
        .finish()
        .expect("Failed to build auth rate limiter");

    let session_rate_limit = GovernorConfigBuilder::default()
        .seconds_per_request(30)
        .burst_size(3)
        .finish()
        .expect("Failed to build session rate limiter");

    let checkout_rate_limit = GovernorConfigBuilder::default()
        .seconds_per_request(2)
        .burst_size(10)
        .finish()
        .expect("Failed to build checkout rate limiter");

    let public_read_limit = GovernorConfigBuilder::default()
        .seconds_per_request(2)
        .burst_size(15)
        .finish()
        .expect("Failed to build public read rate limiter");

    cfg.service(
        web::scope("/api")
            .route("/health", web::get().to(system::health))
            .service(
                web::scope("/merchants")
                    .route("", web::post().to(merchants::create))
                    .route("/me", web::get().to(auth::me))
                    .route("/me", web::patch().to(auth::update_me))
                    .route("/me/invoices", web::get().to(auth::my_invoices))
                    .route(
                        "/me/regenerate-api-key",
                        web::post().to(auth::regenerate_api_key),
                    )
                    .route(
                        "/me/regenerate-dashboard-token",
                        web::post().to(auth::regenerate_dashboard_token),
                    )
                    .route(
                        "/me/regenerate-webhook-secret",
                        web::post().to(auth::regenerate_webhook_secret),
                    )
                    .route(
                        "/me/billing",
                        web::get().to(billing_routes::billing_summary),
                    )
                    .route(
                        "/me/billing/history",
                        web::get().to(billing_routes::billing_history),
                    )
                    .route(
                        "/me/billing/settle",
                        web::post().to(billing_routes::billing_settle),
                    )
                    .route("/me/delete", web::post().to(billing_routes::delete_account))
                    .route("/me/webhooks", web::get().to(auth::my_webhooks))
                    .route("/me/x402/history", web::get().to(x402::history))
                    .route("/me/sessions", web::get().to(sessions::history)),
            )
            .service(
                web::scope("/auth")
                    .wrap(Governor::new(&auth_rate_limit))
                    .route("/session", web::post().to(auth::create_session))
                    .route("/logout", web::post().to(auth::logout))
                    .route("/recover", web::post().to(auth::recover))
                    .route("/recover/confirm", web::post().to(auth::recover_confirm)),
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
            .route(
                "/subscriptions/{id}/cancel",
                web::post().to(subscriptions::cancel),
            )
            .route(
                "/subscriptions/{id}/simulate-period-end",
                web::post().to(subscriptions::simulate_period_end),
            )
            .route(
                "/subscriptions/trigger-renewals",
                web::post().to(subscriptions::trigger_renewals),
            )
            // Payment links (merchant auth)
            .route("/payment-links", web::post().to(payment_links::create))
            .route("/payment-links", web::get().to(payment_links::list))
            .route(
                "/payment-links/{id}",
                web::patch().to(payment_links::update),
            )
            .route(
                "/payment-links/{id}",
                web::delete().to(payment_links::delete),
            )
            .route(
                "/payment-links/{slug}/checkout",
                web::post()
                    .to(payment_links::resolve)
                    .wrap(Governor::new(&checkout_rate_limit)),
            )
            .route(
                "/payment-links/{slug}/info",
                web::get()
                    .to(payment_links::info)
                    .wrap(Governor::new(&public_read_limit)),
            )
            // Donation links (merchant auth)
            .route(
                "/donation-links",
                web::post().to(payment_links::create_donation),
            )
            // Buyer checkout (public)
            .route("/checkout", web::post().to(checkout))
            // Invoice endpoints (API key auth)
            .route("/invoices", web::post().to(invoices::create))
            .route("/invoices", web::get().to(invoice_routes::list_invoices))
            .route(
                "/invoices/lookup/{memo_code}",
                web::get().to(invoice_routes::lookup_by_memo),
            )
            .route("/invoices/{id}", web::get().to(invoices::get))
            .route("/invoices/{id}/status", web::get().to(status::get))
            .route(
                "/invoices/{id}/stream",
                web::get().to(invoice_routes::invoice_stream),
            )
            .route(
                "/invoices/{id}/finalize",
                web::post().to(invoices::finalize),
            )
            .route(
                "/invoices/{id}/cancel",
                web::post().to(invoice_routes::cancel_invoice),
            )
            .route(
                "/invoices/{id}/refund",
                web::post().to(invoice_routes::refund_invoice),
            )
            .route(
                "/invoices/{id}/refund-address",
                web::patch().to(invoice_routes::update_refund_address),
            )
            .route("/invoices/{id}/qr", web::get().to(invoice_routes::qr_code))
            // Ticket endpoints
            .route(
                "/tickets/invoice/{invoice_id}",
                web::get().to(tickets::by_invoice),
            )
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
            // Session endpoints (agentic prepaid credit)
            .service(
                web::scope("/sessions")
                    .route(
                        "/prepare",
                        web::post()
                            .to(sessions::prepare)
                            .wrap(Governor::new(&session_rate_limit)),
                    )
                    .route(
                        "/open",
                        web::post()
                            .to(sessions::open)
                            .wrap(Governor::new(&session_rate_limit)),
                    )
                    .route("/validate", web::get().to(sessions::validate))
                    .route("/deduct", web::post().to(sessions::deduct))
                    .route("/{id}", web::get().to(sessions::get_status))
                    .route("/{id}/close", web::post().to(sessions::close)),
            )
            // Admin endpoints (protected by ADMIN_KEY)
            .route("/admin/auth", web::post().to(admin::auth_check))
            .route("/admin/stats", web::get().to(admin::stats))
            .route("/admin/merchants", web::get().to(admin::merchants))
            .route("/admin/billing", web::get().to(admin::billing))
            .route("/admin/webhooks", web::get().to(admin::webhooks))
            .route("/admin/system", web::get().to(admin::system))
            .route("/admin/test-email", web::post().to(admin::test_email)),
    );

    cfg.route(
        "/.well-known/payment",
        web::get().to(system::well_known_payment),
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
    let (
        product,
        checkout_amount,
        checkout_currency,
        resolved_price_id,
        resolved_price_label,
        resolved_max_qty,
    ) = if let Some(ref price_id) = body.price_id {
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
        (
            product,
            price.unit_amount,
            price.currency.clone(),
            Some(price.id),
            price.label,
            mq,
        )
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
        (
            product,
            price.unit_amount,
            price.currency.clone(),
            Some(price.id),
            price.label,
            mq,
        )
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
                "SELECT COUNT(*) FROM tickets WHERE price_id = ? AND status != 'void'",
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
    let event_row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, event_date FROM events WHERE product_id = ? LIMIT 1")
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
                        let _ = sqlx::query(
                            "UPDATE products SET active = 0 WHERE id = ? AND active = 1",
                        )
                        .bind(&pid)
                        .execute(pool_bg.get_ref())
                        .await;
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

    let merchant = match crate::merchants::get_merchant_by_id(
        pool.get_ref(),
        &product.merchant_id,
        &config.encryption_key,
    )
    .await
    {
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
        if let Ok(status) =
            crate::billing::get_merchant_billing_status(pool.get_ref(), &merchant.id).await
        {
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
        config
            .fee_address
            .as_ref()
            .map(|addr| crate::invoices::FeeConfig {
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
                let enc_name = body
                    .attendee_name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map(|n| {
                        if enc_key.is_empty() {
                            Ok(n.to_string())
                        } else {
                            crate::crypto::encrypt(n, enc_key)
                        }
                    })
                    .transpose()
                    .ok()
                    .flatten();
                let enc_email = body
                    .attendee_email
                    .as_deref()
                    .filter(|e| !e.is_empty())
                    .map(|e| {
                        if enc_key.is_empty() {
                            Ok(e.to_string())
                        } else {
                            crate::crypto::encrypt(e, enc_key)
                        }
                    })
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
                obj.insert(
                    "price_label".to_string(),
                    serde_json::to_value(resolved_price_label).unwrap_or(serde_json::Value::Null),
                );
                if let Ok(Some(ctx)) =
                    crate::events::get_event_context_by_product(pool.get_ref(), &product.id).await
                {
                    obj.insert(
                        "event_title".to_string(),
                        serde_json::Value::String(ctx.event_title),
                    );
                    obj.insert(
                        "event_date".to_string(),
                        serde_json::to_value(ctx.event_date).unwrap_or(serde_json::Value::Null),
                    );
                    obj.insert(
                        "event_location".to_string(),
                        serde_json::to_value(ctx.event_location).unwrap_or(serde_json::Value::Null),
                    );
                }
                obj.insert("is_luma".to_string(), serde_json::json!(is_luma_event));

                let frontend_url = config
                    .frontend_url
                    .as_deref()
                    .unwrap_or("https://cipherpay.app");
                let mut checkout_url = format!("{}/pay/{}", frontend_url, resp.invoice_id);
                if let Some(ref url) = body.success_url {
                    let encoded: String = url
                        .chars()
                        .map(|c| match c {
                            '&' | '=' | '?' | '#' | ' ' => format!("%{:02X}", c as u8),
                            _ => c.to_string(),
                        })
                        .collect();
                    checkout_url = format!("{}?return_url={}", checkout_url, encoded);
                }
                obj.insert(
                    "checkout_url".to_string(),
                    serde_json::Value::String(checkout_url),
                );
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
    success_url: Option<String>,
}

fn validate_checkout(req: &CheckoutRequest) -> Result<(), crate::validation::ValidationError> {
    if req.product_id.is_none() && req.price_id.is_none() {
        return Err(crate::validation::ValidationError::invalid(
            "product_id",
            "either product_id or price_id is required",
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
    if let Some(ref url) = req.success_url {
        crate::validation::validate_length("success_url", url, 2000)?;
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(crate::validation::ValidationError::invalid(
                "success_url",
                "must be a valid HTTP(S) URL",
            ));
        }
    }
    Ok(())
}
