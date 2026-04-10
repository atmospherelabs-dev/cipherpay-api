use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::subscriptions::{self, CreateSubscriptionRequest};
use crate::config::Config;

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreateSubscriptionRequest>,
) -> HttpResponse {
    let merchant = match super::auth::require_merchant_or_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    match subscriptions::create_subscription(pool.get_ref(), &merchant.id, &body).await {
        Ok(sub) => HttpResponse::Created().json(sub),
        Err(e) => HttpResponse::BadRequest().json(serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn list(req: HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    let merchant = match super::auth::require_merchant_or_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
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
    config: web::Data<Config>,
    path: web::Path<String>,
    body: web::Json<CancelBody>,
) -> HttpResponse {
    let merchant = match super::auth::require_merchant_or_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    let sub_id = path.into_inner();
    let at_period_end = body.at_period_end.unwrap_or(false);

    match subscriptions::cancel_subscription(pool.get_ref(), &sub_id, &merchant.id, at_period_end)
        .await
    {
        Ok(Some(sub)) => {
            // Dispatch webhook for immediate cancels (at_period_end=false and status is now canceled)
            // Period-end cancels are handled by the hourly process_renewals job when they actually cancel
            if !at_period_end && sub.status == "canceled" {
                let http = reqwest::Client::new();
                let payload = serde_json::json!({
                    "subscription_id": sub.id,
                    "price_id": sub.price_id,
                    "immediate": true,
                });
                let _ = crate::webhooks::dispatch_event(
                    pool.get_ref(),
                    &http,
                    &merchant.id,
                    "subscription.canceled",
                    payload,
                    &config.encryption_key,
                )
                .await;
            }
            HttpResponse::Ok().json(sub)
        }
        Ok(None) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": "Subscription not found"}))
        }
        Err(e) => HttpResponse::BadRequest().json(serde_json::json!({"error": e.to_string()})),
    }
}

/// Testnet-only: simulate subscription period ending (fast-forward for testing)
/// POST /api/subscriptions/{id}/simulate-period-end
#[derive(serde::Deserialize)]
pub struct SimulateBody {
    /// If true, also simulate a confirmed payment (triggers renewal webhook)
    pub with_payment: Option<bool>,
}

pub async fn simulate_period_end(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    path: web::Path<String>,
    body: web::Json<SimulateBody>,
) -> HttpResponse {
    if !config.is_testnet() {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Simulation endpoints are only available on testnet"
        }));
    }

    let merchant = match super::auth::require_merchant_or_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    let sub_id = path.into_inner();

    // Verify subscription belongs to merchant
    let sub = match subscriptions::get_subscription(pool.get_ref(), &sub_id).await {
        Ok(Some(s)) if s.merchant_id == merchant.id => s,
        Ok(Some(_)) => {
            return HttpResponse::Forbidden().json(serde_json::json!({
                "error": "Subscription does not belong to this merchant"
            }));
        }
        Ok(None) => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Subscription not found"
            }));
        }
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": e.to_string()
            }));
        }
    };

    if sub.status != "active" {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Subscription is {}, not active", sub.status)
        }));
    }

    // Fast-forward: set current_period_end to 1 hour ago
    let past = (chrono::Utc::now() - chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    if let Err(e) = sqlx::query("UPDATE subscriptions SET current_period_end = ? WHERE id = ?")
        .bind(&past)
        .bind(&sub_id)
        .execute(pool.get_ref())
        .await
    {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": e.to_string()
        }));
    }

    let with_payment = body.with_payment.unwrap_or(false);

    if with_payment {
        // Simulate a confirmed payment: advance the period and fire subscription.renewed
        match subscriptions::advance_subscription_period(pool.get_ref(), &sub_id).await {
            Ok(Some(new_sub)) => {
                let http = reqwest::Client::new();
                let payload = serde_json::json!({
                    "subscription_id": new_sub.id,
                    "simulated": true,
                    "new_period_start": new_sub.current_period_start,
                    "new_period_end": new_sub.current_period_end,
                });
                let _ = crate::webhooks::dispatch_event(
                    pool.get_ref(),
                    &http,
                    &merchant.id,
                    "subscription.renewed",
                    payload,
                    &config.encryption_key,
                )
                .await;

                return HttpResponse::Ok().json(serde_json::json!({
                    "message": "Period ended and payment simulated — subscription.renewed webhook fired",
                    "subscription": new_sub,
                }));
            }
            Ok(None) => {
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to advance subscription"
                }));
            }
            Err(e) => {
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": e.to_string()
                }));
            }
        }
    }

    // No payment simulation — just fast-forward and let the hourly job mark it past_due
    let updated_sub = subscriptions::get_subscription(pool.get_ref(), &sub_id)
        .await
        .ok()
        .flatten();

    HttpResponse::Ok().json(serde_json::json!({
        "message": "Period fast-forwarded to past. Run process_renewals or wait for hourly job to mark past_due.",
        "subscription": updated_sub,
        "hint": "Use with_payment: true to simulate a confirmed renewal payment"
    }))
}
