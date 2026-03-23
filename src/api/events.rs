use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::events::CreateEventRequest;

pub async fn list(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    match crate::events::list_events_for_merchant(pool.get_ref(), &merchant.id).await {
        Ok(events) => HttpResponse::Ok().json(events),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list events");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreateEventRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    match crate::events::create_event_with_product_and_prices(pool.get_ref(), &merchant.id, &body).await {
        Ok(event) => HttpResponse::Created().json(event),
        Err(e) => {
            let msg = e.to_string();
            let is_validation = msg.contains("is required")
                || msg.contains("must be > 0")
                || msg.contains("Unsupported currency");
            if is_validation {
                HttpResponse::BadRequest().json(serde_json::json!({
                    "error": msg
                }))
            } else {
                tracing::error!(error = %e, "Failed to create event");
                HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }))
            }
        }
    }
}

pub async fn archive(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let event_id = path.into_inner();
    match crate::events::archive_event(pool.get_ref(), &merchant.id, &event_id).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({ "status": "cancelled" })),
        Ok(false) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Event not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to archive event");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}
