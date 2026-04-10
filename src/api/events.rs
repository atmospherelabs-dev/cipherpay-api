use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::events::{CreateEventRequest, UpdateEventRequest};

pub async fn list(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
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
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    match crate::events::create_event_with_product_and_prices(pool.get_ref(), &merchant.id, &body)
        .await
    {
        Ok(event) => HttpResponse::Created().json(event),
        Err(e) => {
            if e.downcast_ref::<crate::events::EventValidationError>()
                .is_some()
            {
                HttpResponse::BadRequest().json(serde_json::json!({
                    "error": e.to_string()
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

pub async fn get(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    let event_id = path.into_inner();
    match crate::events::get_event_detail(pool.get_ref(), &merchant.id, &event_id).await {
        Ok(Some(detail)) => HttpResponse::Ok().json(detail),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Event not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get event");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn update(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
    body: web::Json<UpdateEventRequest>,
) -> HttpResponse {
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    let event_id = path.into_inner();
    match crate::events::update_event(pool.get_ref(), &merchant.id, &event_id, &body).await {
        Ok(Some(event)) => HttpResponse::Ok().json(event),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Event not found"
        })),
        Err(e) => {
            if e.downcast_ref::<crate::events::EventValidationError>()
                .is_some()
            {
                HttpResponse::BadRequest().json(serde_json::json!({
                    "error": e.to_string()
                }))
            } else {
                tracing::error!(error = %e, "Failed to update event");
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
    let merchant = match super::auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
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
