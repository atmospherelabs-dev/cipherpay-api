use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::prices::{self, CreatePriceRequest, UpdatePriceRequest};

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreatePriceRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    match crate::events::is_product_backed_by_event(pool.get_ref(), &body.product_id).await {
        Ok(true) => {
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": "This product is managed by an Event. Use /api/events endpoints."
            }));
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "Failed to check event-backed product");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    }

    if body.unit_amount <= 0.0 {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "unit_amount must be > 0"
        }));
    }

    match prices::create_price(pool.get_ref(), &merchant.id, &body).await {
        Ok(price) => HttpResponse::Created().json(price),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint") {
                HttpResponse::Conflict().json(serde_json::json!({
                    "error": "A price for this currency already exists on this product"
                }))
            } else {
                tracing::error!(error = %e, "Failed to create price");
                HttpResponse::BadRequest().json(serde_json::json!({
                    "error": msg
                }))
            }
        }
    }
}

pub async fn list(
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

    let product_id = path.into_inner();

    let product = match crate::products::get_product(pool.get_ref(), &product_id).await {
        Ok(Some(p)) if p.merchant_id == merchant.id => p,
        _ => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Product not found"
            }));
        }
    };

    match prices::list_prices_for_product(pool.get_ref(), &product.id).await {
        Ok(prices) => HttpResponse::Ok().json(prices),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list prices");
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
    body: web::Json<UpdatePriceRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    let price_id = path.into_inner();

    let existing = match prices::get_price(pool.get_ref(), &price_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Price not found"
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch price for update");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };
    match crate::events::is_product_backed_by_event(pool.get_ref(), &existing.product_id).await {
        Ok(true) => {
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": "This product is managed by an Event. Use /api/events endpoints."
            }));
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "Failed to check event-backed product");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    }

    match prices::update_price(pool.get_ref(), &price_id, &merchant.id, &body).await {
        Ok(Some(price)) => HttpResponse::Ok().json(price),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Price not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update price");
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": e.to_string()
            }))
        }
    }
}

pub async fn deactivate(
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

    let price_id = path.into_inner();

    let existing = match prices::get_price(pool.get_ref(), &price_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Price not found"
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch price for delete");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    };
    match crate::events::is_product_backed_by_event(pool.get_ref(), &existing.product_id).await {
        Ok(true) => {
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": "This product is managed by an Event. Use /api/events endpoints."
            }));
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "Failed to check event-backed product");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }));
        }
    }

    match prices::deactivate_price(pool.get_ref(), &price_id, &merchant.id).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({ "status": "deactivated" })),
        Ok(false) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Price not found"
        })),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("last active price") {
                HttpResponse::BadRequest().json(serde_json::json!({
                    "error": msg
                }))
            } else {
                tracing::error!(error = %e, "Failed to deactivate price");
                HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }))
            }
        }
    }
}

/// Public endpoint: get a price by ID for buyers
pub async fn get_public(pool: web::Data<SqlitePool>, path: web::Path<String>) -> HttpResponse {
    let price_id = path.into_inner();

    match prices::get_price(pool.get_ref(), &price_id).await {
        Ok(Some(price)) if price.active == 1 => HttpResponse::Ok().json(serde_json::json!({
            "id": price.id,
            "product_id": price.product_id,
            "currency": price.currency,
            "unit_amount": price.unit_amount,
            "label": price.label,
        })),
        _ => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Price not found"
        })),
    }
}
