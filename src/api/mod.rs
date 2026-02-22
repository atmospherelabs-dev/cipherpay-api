pub mod auth;
pub mod invoices;
pub mod merchants;
pub mod products;
pub mod rates;
pub mod status;

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
                    .wrap(Governor::new(&auth_rate_limit))
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
            // Buyer checkout (public)
            .route("/checkout", web::post().to(checkout))
            // Invoice endpoints (API key auth)
            .route("/invoices", web::post().to(invoices::create))
            .route("/invoices", web::get().to(list_invoices))
            .route("/invoices/lookup/{memo_code}", web::get().to(lookup_by_memo))
            .route("/invoices/{id}", web::get().to(invoices::get))
            .route("/invoices/{id}/status", web::get().to(status::get))
            .route("/invoices/{id}/stream", web::get().to(invoice_stream))
            .route(
                "/invoices/{id}/simulate-detect",
                web::post().to(simulate_detect),
            )
            .route(
                "/invoices/{id}/simulate-confirm",
                web::post().to(simulate_confirm),
            )
            .route("/invoices/{id}/cancel", web::post().to(cancel_invoice))
            .route("/invoices/{id}/refund", web::post().to(refund_invoice))
            .route("/invoices/{id}/qr", web::get().to(qr_code))
            .route("/rates", web::get().to(rates::get)),
    );
}

/// Public checkout endpoint for buyer-driven invoice creation.
/// Buyer selects a product, provides variant + shipping, invoice is created with server-side pricing.
async fn checkout(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
    body: web::Json<CheckoutRequest>,
) -> actix_web::HttpResponse {
    if let Err(e) = validate_checkout(&body) {
        return actix_web::HttpResponse::BadRequest().json(e.to_json());
    }

    let product = match crate::products::get_product(pool.get_ref(), &body.product_id).await {
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

    if let Some(ref variant) = body.variant {
        let valid_variants = product.variants_list();
        if !valid_variants.is_empty() && !valid_variants.contains(variant) {
            return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Invalid variant",
                "valid_variants": valid_variants,
            }));
        }
    }

    let merchant = match crate::merchants::get_all_merchants(pool.get_ref(), &config.encryption_key).await {
        Ok(merchants) => match merchants.into_iter().find(|m| m.id == product.merchant_id) {
            Some(m) => m,
            None => {
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Merchant not found"
                }));
            }
        },
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
        product_name: Some(product.name.clone()),
        size: body.variant.clone(),
        price_eur: product.price_eur,
        currency: Some(product.currency.clone()),
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
        rates.zec_eur,
        rates.zec_usd,
        config.invoice_expiry_minutes,
        fee_config.as_ref(),
    )
    .await
    {
        Ok(resp) => actix_web::HttpResponse::Created().json(resp),
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
    product_id: String,
    variant: Option<String>,
    refund_address: Option<String>,
}

fn validate_checkout(req: &CheckoutRequest) -> Result<(), crate::validation::ValidationError> {
    crate::validation::validate_length("product_id", &req.product_id, 100)?;
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
        "version": env!("CARGO_PKG_VERSION"),
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
        "SELECT id, merchant_id, memo_code, product_name, size,
         price_eur, price_usd, currency, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         status, detected_txid,
         detected_at, expires_at, confirmed_at, refunded_at,
         refund_address, created_at
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
                    serde_json::json!({
                        "id": r.get::<String, _>("id"),
                        "merchant_id": r.get::<String, _>("merchant_id"),
                        "memo_code": r.get::<String, _>("memo_code"),
                        "product_name": r.get::<Option<String>, _>("product_name"),
                        "size": r.get::<Option<String>, _>("size"),
                        "price_eur": r.get::<f64, _>("price_eur"),
                        "price_usd": r.get::<Option<f64>, _>("price_usd"),
                        "currency": r.get::<Option<String>, _>("currency"),
                        "price_zec": r.get::<f64, _>("price_zec"),
                        "zec_rate": r.get::<f64, _>("zec_rate_at_creation"),
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
        Ok(Some(inv)) => actix_web::HttpResponse::Ok().json(serde_json::json!({
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
        })),
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
            });
            let _ = tx
                .send(sse::Data::new(data.to_string()).event("status").into())
                .await;
        }

        loop {
            tick.tick().await;

            match crate::invoices::get_invoice_status(&pool, &invoice_id).await {
                Ok(Some(status)) => {
                    if status.status != last_status {
                        last_status.clone_from(&status.status);
                        let data = serde_json::json!({
                            "status": status.status,
                            "txid": status.detected_txid,
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

/// Test endpoint: simulate payment detection (testnet only)
async fn simulate_detect(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    if !config.is_testnet() {
        return actix_web::HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Simulation endpoints disabled in production"
        }));
    }

    let invoice_id = path.into_inner();
    let fake_txid = format!("sim_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));

    match crate::invoices::mark_detected(pool.get_ref(), &invoice_id, &fake_txid).await {
        Ok(()) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "status": "detected",
            "txid": fake_txid,
            "message": "Simulated payment detection"
        })),
        Err(e) => actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("{}", e)
        })),
    }
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
            if let Err(e) = crate::invoices::mark_expired(pool.get_ref(), &invoice_id).await {
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("{}", e)
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
        Ok(Some(inv)) if inv.merchant_id == merchant.id && inv.status == "confirmed" => {
            if let Err(e) = crate::invoices::mark_refunded(pool.get_ref(), &invoice_id).await {
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("{}", e)
                }));
            }
            let response = serde_json::json!({
                "status": "refunded",
                "refund_address": inv.refund_address,
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

/// Test endpoint: simulate payment confirmation (testnet only)
async fn simulate_confirm(
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    if !config.is_testnet() {
        return actix_web::HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Simulation endpoints disabled in production"
        }));
    }

    let invoice_id = path.into_inner();

    match crate::invoices::mark_confirmed(pool.get_ref(), &invoice_id).await {
        Ok(()) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "status": "confirmed",
            "message": "Simulated payment confirmation"
        })),
        Err(e) => actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("{}", e)
        })),
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

    match crate::billing::create_settlement_invoice(
        pool.get_ref(), &merchant.id, summary.outstanding_zec, &fee_address,
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
