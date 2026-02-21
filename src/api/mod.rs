pub mod auth;
pub mod invoices;
pub mod merchants;
pub mod products;
pub mod rates;
pub mod status;

use actix_web::web;
use actix_web_lab::sse;
use base64::Engine;
use sqlx::SqlitePool;
use std::time::Duration;
use tokio::time::interval;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api")
            .route("/health", web::get().to(health))
            // Merchant registration (public)
            .route("/merchants", web::post().to(merchants::create))
            // Auth / session management
            .route("/auth/session", web::post().to(auth::create_session))
            .route("/auth/logout", web::post().to(auth::logout))
            .route("/auth/recover", web::post().to(auth::recover))
            .route("/auth/recover/confirm", web::post().to(auth::recover_confirm))
            // Dashboard endpoints (cookie auth)
            .route("/merchants/me", web::get().to(auth::me))
            .route("/merchants/me", web::patch().to(auth::update_me))
            .route("/merchants/me/invoices", web::get().to(auth::my_invoices))
            .route("/merchants/me/regenerate-api-key", web::post().to(auth::regenerate_api_key))
            .route("/merchants/me/regenerate-dashboard-token", web::post().to(auth::regenerate_dashboard_token))
            .route("/merchants/me/regenerate-webhook-secret", web::post().to(auth::regenerate_webhook_secret))
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
            .route("/invoices/{id}/ship", web::post().to(ship_invoice))
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

    let merchant = match crate::merchants::get_all_merchants(pool.get_ref()).await {
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
        shipping_alias: body.shipping_alias.clone(),
        shipping_address: body.shipping_address.clone(),
        shipping_region: body.shipping_region.clone(),
        refund_address: body.refund_address.clone(),
    };

    match crate::invoices::create_invoice(
        pool.get_ref(),
        &merchant.id,
        &merchant.payment_address,
        &invoice_req,
        rates.zec_eur,
        config.invoice_expiry_minutes,
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
    shipping_alias: Option<String>,
    shipping_address: Option<String>,
    shipping_region: Option<String>,
    refund_address: Option<String>,
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
                    match crate::merchants::authenticate(&pool, key).await {
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

    let rows = sqlx::query_as::<_, (
        String, String, String, Option<String>, Option<String>,
        f64, f64, f64, String, String,
        String, Option<String>,
        Option<String>, String, Option<String>, String,
    )>(
        "SELECT id, merchant_id, memo_code, product_name, size,
         price_eur, price_zec, zec_rate_at_creation, payment_address, zcash_uri,
         status, detected_txid,
         detected_at, expires_at, confirmed_at, created_at
         FROM invoices WHERE merchant_id = ? ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(rows) => {
            let invoices: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.0, "merchant_id": r.1, "memo_code": r.2,
                        "product_name": r.3, "size": r.4, "price_eur": r.5,
                        "price_zec": r.6, "zec_rate": r.7,
                        "payment_address": r.8, "zcash_uri": r.9,
                        "status": r.10,
                        "detected_txid": r.11, "detected_at": r.12,
                        "expires_at": r.13, "confirmed_at": r.14, "created_at": r.15,
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
        Ok(Some(invoice)) => actix_web::HttpResponse::Ok().json(invoice),
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

    let merchant = match crate::merchants::get_all_merchants(pool.get_ref()).await {
        Ok(merchants) => match merchants.into_iter().find(|m| m.id == invoice.merchant_id) {
            Some(m) => m,
            None => return actix_web::HttpResponse::NotFound().finish(),
        },
        _ => return actix_web::HttpResponse::InternalServerError().finish(),
    };

    let memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(invoice.memo_code.as_bytes());
    let uri = format!(
        "zcash:{}?amount={:.8}&memo={}",
        merchant.payment_address, invoice.price_zec, memo_b64
    );

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

/// Mark a confirmed invoice as shipped (dashboard auth)
async fn ship_invoice(
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
            if let Err(e) = crate::invoices::mark_shipped(pool.get_ref(), &invoice_id).await {
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("{}", e)
                }));
            }
            actix_web::HttpResponse::Ok().json(serde_json::json!({ "status": "shipped" }))
        }
        Ok(Some(_)) => {
            actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Only confirmed invoices can be marked as shipped"
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
