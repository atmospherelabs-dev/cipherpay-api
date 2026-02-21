pub mod invoices;
pub mod merchants;
pub mod status;
pub mod rates;

use actix_web::web;
use sqlx::SqlitePool;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api")
            .route("/health", web::get().to(health))
            .route("/merchants", web::post().to(merchants::create))
            .route("/invoices", web::post().to(invoices::create))
            .route("/invoices", web::get().to(list_invoices))
            .route("/invoices/lookup/{memo_code}", web::get().to(lookup_by_memo))
            .route("/invoices/{id}", web::get().to(invoices::get))
            .route("/invoices/{id}/status", web::get().to(status::get))
            .route("/invoices/{id}/simulate-detect", web::post().to(simulate_detect))
            .route("/invoices/{id}/simulate-confirm", web::post().to(simulate_confirm))
            .route("/invoices/{id}/qr", web::get().to(qr_code))
            .route("/rates", web::get().to(rates::get)),
    );
}

async fn health() -> actix_web::HttpResponse {
    actix_web::HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "service": "cipherpay",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn list_invoices(pool: web::Data<SqlitePool>) -> actix_web::HttpResponse {
    let rows = sqlx::query_as::<_, (
        String, String, String, Option<String>, Option<String>,
        f64, f64, f64, String, Option<String>,
        Option<String>, String, Option<String>, String,
    )>(
        "SELECT id, merchant_id, memo_code, product_name, size,
         price_eur, price_zec, zec_rate_at_creation, status, detected_txid,
         detected_at, expires_at, confirmed_at, created_at
         FROM invoices ORDER BY created_at DESC LIMIT 50"
    )
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(rows) => {
            let invoices: Vec<_> = rows.into_iter().map(|r| serde_json::json!({
                "id": r.0, "merchant_id": r.1, "memo_code": r.2,
                "product_name": r.3, "size": r.4, "price_eur": r.5,
                "price_zec": r.6, "zec_rate": r.7, "status": r.8,
                "detected_txid": r.9, "detected_at": r.10,
                "expires_at": r.11, "confirmed_at": r.12, "created_at": r.13,
            })).collect();
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

/// Look up an invoice by memo code (e.g. GET /api/invoices/lookup/CP-C6CDB775)
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

/// Generate a QR code PNG for a zcash: payment URI
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

    let uri = format!(
        "zcash:{}?amount={:.8}&memo={}",
        merchant.payment_address,
        invoice.price_zec,
        hex::encode(invoice.memo_code.as_bytes())
    );

    match generate_qr_png(&uri) {
        Ok(png_bytes) => actix_web::HttpResponse::Ok()
            .content_type("image/png")
            .body(png_bytes),
        Err(_) => actix_web::HttpResponse::InternalServerError().finish(),
    }
}

fn generate_qr_png(data: &str) -> anyhow::Result<Vec<u8>> {
    use qrcode::QrCode;
    use image::Luma;

    let code = QrCode::new(data.as_bytes())?;
    let img = code.render::<Luma<u8>>()
        .quiet_zone(true)
        .min_dimensions(250, 250)
        .build();

    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)?;
    Ok(buf.into_inner())
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
