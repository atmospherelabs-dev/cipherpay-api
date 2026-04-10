use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::SqlitePool;

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub code: String,
}

/// Public endpoint: returns the ticket code, status, and event metadata for the checkout receipt.
/// No auth required — invoice IDs are unguessable UUIDs (same model as refund_address).
pub async fn by_invoice(pool: web::Data<SqlitePool>, path: web::Path<String>) -> HttpResponse {
    let invoice_id = path.into_inner();
    match crate::tickets::get_ticket_by_invoice(pool.get_ref(), &invoice_id).await {
        Ok(Some(ticket)) => {
            let event = sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT event_date, event_location FROM events WHERE product_id = ? AND status != 'cancelled' LIMIT 1"
            )
            .bind(&ticket.product_id)
            .fetch_optional(pool.get_ref())
            .await
            .ok()
            .flatten();

            let price_label: Option<String> = if let Some(ref pid) = ticket.price_id {
                sqlx::query_scalar("SELECT label FROM prices WHERE id = ?")
                    .bind(pid)
                    .fetch_optional(pool.get_ref())
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };

            let mut resp = serde_json::json!({
                "code": ticket.code,
                "status": ticket.status
            });
            if let Some((date, location)) = event {
                resp["event_date"] = serde_json::json!(date);
                resp["event_location"] = serde_json::json!(location);
            }
            if let Some(label) = price_label {
                resp["price_label"] = serde_json::json!(label);
            }
            HttpResponse::Ok().json(resp)
        }
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
    let merchant = match super::auth::require_merchant_or_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
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

pub async fn list(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
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
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
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
