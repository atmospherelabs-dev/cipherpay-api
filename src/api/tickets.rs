use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::SqlitePool;

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub code: String,
}

/// Public endpoint: returns only the ticket code and status for the checkout receipt.
/// No auth required — invoice IDs are unguessable UUIDs (same model as refund_address).
pub async fn by_invoice(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let invoice_id = path.into_inner();
    match crate::tickets::get_ticket_by_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(ticket)) => HttpResponse::Ok().json(serde_json::json!({
            "code": ticket.code,
            "status": ticket.status
        })),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Ticket not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to load ticket by invoice");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn scan(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<ScanRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_merchant_or_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }))
        }
    };

    let code = body.code.trim();
    if code.is_empty() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "code is required"
        }));
    }

    match crate::tickets::scan_ticket(pool.get_ref(), code, &merchant.id).await {
        Ok(Some((ticket, just_used))) => {
            if ticket.merchant_id != merchant.id {
                return HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Ticket does not belong to this merchant"
                }));
            }
            HttpResponse::Ok().json(serde_json::json!({
                "valid": just_used,
                "already_used": ticket.status == "used" && !just_used,
                "voided": ticket.status == "void",
                "ticket_status": ticket.status,
                "ticket_id": ticket.id,
                "invoice_id": ticket.invoice_id,
                "product_id": ticket.product_id,
                "price_id": ticket.price_id,
                "used_at": ticket.used_at
            }))
        }
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invalid ticket"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to scan ticket");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn list(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }))
        }
    };

    match crate::tickets::list_tickets_for_merchant(pool.get_ref(), &merchant.id).await {
        Ok(rows) => HttpResponse::Ok().json(rows),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list tickets");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn void(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }))
        }
    };

    let ticket_id = path.into_inner();
    match crate::tickets::void_ticket(pool.get_ref(), &ticket_id, &merchant.id).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({ "status": "void" })),
        Ok(false) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Ticket not found or cannot be voided"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to void ticket");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}
