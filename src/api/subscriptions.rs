use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::subscriptions::{self, CreateSubscriptionRequest};

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreateSubscriptionRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({"error": "Not authenticated"})),
    };

    match subscriptions::create_subscription(pool.get_ref(), &merchant.id, &body).await {
        Ok(sub) => HttpResponse::Created().json(sub),
        Err(e) => HttpResponse::BadRequest().json(serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn list(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({"error": "Not authenticated"})),
    };

    match subscriptions::list_subscriptions(pool.get_ref(), &merchant.id).await {
        Ok(subs) => HttpResponse::Ok().json(subs),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list subscriptions");
            HttpResponse::InternalServerError().json(serde_json::json!({"error": "Internal error"}))
        }
    }
}

#[derive(serde::Deserialize)]
pub struct CancelBody {
    pub at_period_end: Option<bool>,
}

pub async fn cancel(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
    body: web::Json<CancelBody>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => return HttpResponse::Unauthorized().json(serde_json::json!({"error": "Not authenticated"})),
    };

    let sub_id = path.into_inner();
    let at_period_end = body.at_period_end.unwrap_or(false);

    match subscriptions::cancel_subscription(pool.get_ref(), &sub_id, &merchant.id, at_period_end).await {
        Ok(Some(sub)) => HttpResponse::Ok().json(sub),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({"error": "Subscription not found"})),
        Err(e) => HttpResponse::BadRequest().json(serde_json::json!({"error": e.to_string()})),
    }
}
