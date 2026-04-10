use actix_web::web;
use actix_web_lab::sse;
use base64::Engine;
use sqlx::SqlitePool;
use std::time::Duration;
use tokio::time::interval;

use super::auth;

/// List invoices: requires API key or session auth. Scoped to the authenticated merchant.
pub async fn list_invoices(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_api_key_or_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    let rows = sqlx::query(
        "SELECT invoices.id, invoices.merchant_id, memo_code, invoices.product_id, product_name, size,
         price_eur, price_usd, invoices.currency, price_zec, zec_rate_at_creation,
         amount, invoices.price_id,
         payment_address, zcash_uri,
         status, detected_txid,
         detected_at, expires_at, confirmed_at, refunded_at,
         refund_address, invoices.created_at, price_zatoshis, received_zatoshis,
         (EXISTS (SELECT 1 FROM events WHERE product_id = invoices.product_id)) AS is_event,
         pr.label AS price_label,
         invoices.is_donation, invoices.payment_link_id
         FROM invoices
         LEFT JOIN prices pr ON pr.id = invoices.price_id
         WHERE invoices.merchant_id = ? ORDER BY invoices.created_at DESC LIMIT 50",
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
                        "price_label": r.get::<Option<String>, _>("price_label"),
                        "is_donation": r.get::<i32, _>("is_donation") == 1,
                        "payment_link_id": r.get::<Option<String>, _>("payment_link_id"),
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

pub async fn lookup_by_memo(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let memo_code = path.into_inner();

    match crate::invoices::get_invoice_by_memo(pool.get_ref(), &memo_code).await {
        Ok(Some(inv)) => {
            let received_zec = crate::invoices::zatoshis_to_zec(inv.received_zatoshis);
            let overpaid =
                inv.received_zatoshis > inv.price_zatoshis + 1000 && inv.price_zatoshis > 0;
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
        }
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
pub async fn invoice_stream(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> impl actix_web::Responder {
    let invoice_id = path.into_inner();
    let (tx, rx) = tokio::sync::mpsc::channel::<sse::Event>(10);

    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(2));
        let mut last_status = String::new();

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
pub async fn qr_code(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let invoice_id = path.into_inner();

    let invoice = match crate::invoices::get_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(inv)) => inv,
        _ => return actix_web::HttpResponse::NotFound().finish(),
    };

    let uri = if invoice.zcash_uri.is_empty() {
        let memo_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(invoice.memo_code.as_bytes());
        format!(
            "zcash:{}?amount={:.8}&memo={}",
            invoice.payment_address, invoice.price_zec, memo_b64
        )
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
pub async fn cancel_invoice(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
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
        Ok(Some(_)) => actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Only pending invoices can be cancelled"
        })),
        _ => actix_web::HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invoice not found"
        })),
    }
}

/// Mark an invoice as refunded (dashboard auth)
pub async fn refund_invoice(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
    body: web::Json<serde_json::Value>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    let invoice_id = path.into_inner();
    let refund_txid = body
        .get("refund_txid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    match crate::invoices::get_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(inv)) if inv.merchant_id == merchant.id && inv.status == "confirmed" => {
            if let Err(e) =
                crate::invoices::mark_refunded(pool.get_ref(), &invoice_id, refund_txid).await
            {
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
        Ok(Some(_)) => actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Only confirmed invoices can be refunded"
        })),
        _ => actix_web::HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invoice not found"
        })),
    }
}

/// Buyer can save a refund address on their invoice (write-once).
/// No auth required: invoice IDs are unguessable UUIDs and the address
/// can only be set once (write-once guard in update_refund_address).
pub async fn update_refund_address(
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
