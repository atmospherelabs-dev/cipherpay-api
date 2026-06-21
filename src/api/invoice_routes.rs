use actix_web::web;
use actix_web_lab::sse;
use base64::Engine;
use serde::Deserialize;
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
         confirmed_rate, confirmed_fiat_amount,
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
                        "confirmed_rate": r.get::<Option<f64>, _>("confirmed_rate"),
                        "confirmed_fiat_amount": r.get::<Option<f64>, _>("confirmed_fiat_amount"),
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
    let merchant = match auth::require_full_session(&req, pool.get_ref()).await {
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
    let merchant = match auth::require_full_session(&req, pool.get_ref()).await {
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

#[derive(Deserialize)]
pub struct ExportQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub status: Option<String>,
}

/// GET /api/invoices/export/csv — download confirmed invoices as CSV for accounting.
pub async fn export_csv(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    query: web::Query<ExportQuery>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_full_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let status_filter = query.status.as_deref().unwrap_or("confirmed");

    let rows = sqlx::query(
        "SELECT id, memo_code, product_name, currency, amount,
         price_zec, zec_rate_at_creation, price_zatoshis, received_zatoshis,
         confirmed_rate, confirmed_fiat_amount,
         status, detected_txid, created_at, confirmed_at, refunded_at
         FROM invoices
         WHERE merchant_id = ?
           AND status = ?
           AND (? IS NULL OR created_at >= ?)
           AND (? IS NULL OR created_at <= ?)
         ORDER BY created_at ASC",
    )
    .bind(&merchant.id)
    .bind(status_filter)
    .bind(&query.from)
    .bind(&query.from)
    .bind(&query.to)
    .bind(&query.to)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(rows) => {
            use sqlx::Row;
            let mut csv = String::from(
                "id,memo_code,product_name,currency,amount_fiat,price_zec,\
                 zec_rate_at_creation,confirmed_rate,confirmed_fiat_amount,\
                 received_zec,status,txid,created_at,confirmed_at\n",
            );
            for r in &rows {
                let rz = r.get::<i64, _>("received_zatoshis");
                let received_zec = crate::invoices::zatoshis_to_zec(rz);
                let product = r
                    .get::<Option<String>, _>("product_name")
                    .unwrap_or_default()
                    .replace(',', " ");
                let cur = r
                    .get::<Option<String>, _>("currency")
                    .unwrap_or_else(|| "EUR".to_string());
                let amt = r.get::<Option<f64>, _>("amount").unwrap_or(0.0);
                let conf_rate = r.get::<Option<f64>, _>("confirmed_rate");
                let conf_fiat = r.get::<Option<f64>, _>("confirmed_fiat_amount");

                csv.push_str(&format!(
                    "{},{},{},{},{:.2},{:.8},{:.4},{},{},{:.8},{},{},{},{}\n",
                    r.get::<String, _>("id"),
                    r.get::<String, _>("memo_code"),
                    product,
                    cur,
                    amt,
                    r.get::<f64, _>("price_zec"),
                    r.get::<f64, _>("zec_rate_at_creation"),
                    conf_rate.map_or(String::new(), |v| format!("{:.4}", v)),
                    conf_fiat.map_or(String::new(), |v| format!("{:.2}", v)),
                    received_zec,
                    r.get::<String, _>("status"),
                    r.get::<Option<String>, _>("detected_txid").unwrap_or_default(),
                    r.get::<String, _>("created_at"),
                    r.get::<Option<String>, _>("confirmed_at").unwrap_or_default(),
                ));
            }
            actix_web::HttpResponse::Ok()
                .content_type("text/csv; charset=utf-8")
                .insert_header(("Content-Disposition", "attachment; filename=cipherpay-invoices.csv"))
                .body(csv)
        }
        Err(e) => {
            tracing::error!(error = %e, "CSV export failed");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Export failed"
            }))
        }
    }
}

/// GET /api/ledger/{token} — read-only invoice list via shared token (for accountants).
pub async fn ledger_by_token(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let token = path.into_inner();

    let token_row = sqlx::query(
        "SELECT merchant_id, expires_at, revoked_at FROM ledger_tokens WHERE id = ?",
    )
    .bind(&token)
    .fetch_optional(pool.get_ref())
    .await;

    let not_found = || {
        actix_web::HttpResponse::NotFound().json(serde_json::json!({"error": "Invalid or expired token"}))
    };

    let merchant_id = match token_row {
        Ok(Some(row)) => {
            use sqlx::Row;
            if row.get::<Option<String>, _>("revoked_at").is_some() {
                return not_found();
            }
            if let Some(exp) = row.get::<Option<String>, _>("expires_at") {
                if let Ok(exp_dt) = chrono::NaiveDateTime::parse_from_str(&exp, "%Y-%m-%dT%H:%M:%SZ") {
                    if exp_dt < chrono::Utc::now().naive_utc() {
                        return not_found();
                    }
                }
            }
            row.get::<String, _>("merchant_id")
        }
        _ => {
            return not_found();
        }
    };

    let rows = sqlx::query(
        "SELECT id, memo_code, product_name, currency, amount,
         price_zec, zec_rate_at_creation, confirmed_rate, confirmed_fiat_amount,
         price_zatoshis, received_zatoshis,
         status, detected_txid, created_at, confirmed_at
         FROM invoices
         WHERE merchant_id = ? AND status IN ('confirmed', 'refunded')
         ORDER BY created_at DESC LIMIT 500",
    )
    .bind(&merchant_id)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(rows) => {
            use sqlx::Row;
            let items: Vec<_> = rows
                .iter()
                .map(|r| {
                    let rz = r.get::<i64, _>("received_zatoshis");
                    serde_json::json!({
                        "id": r.get::<String, _>("id"),
                        "memo_code": r.get::<String, _>("memo_code"),
                        "product_name": r.get::<Option<String>, _>("product_name"),
                        "currency": r.get::<Option<String>, _>("currency"),
                        "amount_fiat": r.get::<Option<f64>, _>("amount"),
                        "price_zec": r.get::<f64, _>("price_zec"),
                        "zec_rate_at_creation": r.get::<f64, _>("zec_rate_at_creation"),
                        "confirmed_rate": r.get::<Option<f64>, _>("confirmed_rate"),
                        "confirmed_fiat_amount": r.get::<Option<f64>, _>("confirmed_fiat_amount"),
                        "received_zec": crate::invoices::zatoshis_to_zec(rz),
                        "status": r.get::<String, _>("status"),
                        "txid": r.get::<Option<String>, _>("detected_txid"),
                        "created_at": r.get::<String, _>("created_at"),
                        "confirmed_at": r.get::<Option<String>, _>("confirmed_at"),
                    })
                })
                .collect();
            actix_web::HttpResponse::Ok().json(items)
        }
        Err(e) => {
            tracing::error!(error = %e, "Ledger token query failed");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({"error": "Internal error"}))
        }
    }
}

/// POST /api/merchants/me/ledger-tokens — create a shared ledger link
pub async fn create_ledger_token(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<serde_json::Value>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_full_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let label = body
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("Accountant");
    let expires_days = body
        .get("expires_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(90)
        .clamp(1, 365);

    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_tokens WHERE merchant_id = ? AND revoked_at IS NULL",
    )
    .bind(&merchant.id)
    .fetch_one(pool.get_ref())
    .await
    .unwrap_or(0);

    if active_count >= 10 {
        return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Maximum 10 active ledger tokens per merchant"
        }));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let expires_at = (chrono::Utc::now() + chrono::Duration::days(expires_days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let result = sqlx::query(
        "INSERT INTO ledger_tokens (id, merchant_id, label, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&merchant.id)
    .bind(label)
    .bind(&expires_at)
    .execute(pool.get_ref())
    .await;

    match result {
        Ok(_) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "token": id,
            "label": label,
            "expires_at": expires_at,
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create ledger token");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({"error": "Failed to create token"}))
        }
    }
}

/// DELETE /api/merchants/me/ledger-tokens/{token_id} — revoke a shared ledger link
pub async fn revoke_ledger_token(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_full_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let token_id = path.into_inner();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let result = sqlx::query(
        "UPDATE ledger_tokens SET revoked_at = ? WHERE id = ? AND merchant_id = ? AND revoked_at IS NULL",
    )
    .bind(&now)
    .bind(&token_id)
    .bind(&merchant.id)
    .execute(pool.get_ref())
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            actix_web::HttpResponse::Ok().json(serde_json::json!({"status": "revoked"}))
        }
        Ok(_) => actix_web::HttpResponse::NotFound().json(serde_json::json!({"error": "Token not found or already revoked"})),
        Err(e) => {
            tracing::error!(error = %e, "Failed to revoke ledger token");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({"error": "Internal error"}))
        }
    }
}

/// GET /api/merchants/me/ledger-tokens — list active ledger tokens
pub async fn list_ledger_tokens(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_full_session(&req, pool.get_ref()).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let rows = sqlx::query(
        "SELECT id, label, expires_at, created_at, revoked_at FROM ledger_tokens WHERE merchant_id = ? ORDER BY created_at DESC",
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await;

    match rows {
        Ok(rows) => {
            use sqlx::Row;
            let tokens: Vec<_> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.get::<String, _>("id"),
                        "label": r.get::<String, _>("label"),
                        "expires_at": r.get::<Option<String>, _>("expires_at"),
                        "created_at": r.get::<String, _>("created_at"),
                        "revoked": r.get::<Option<String>, _>("revoked_at").is_some(),
                    })
                })
                .collect();
            actix_web::HttpResponse::Ok().json(tokens)
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list ledger tokens");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({"error": "Internal error"}))
        }
    }
}
