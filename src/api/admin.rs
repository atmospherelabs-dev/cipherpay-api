use actix_web::web;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::SqlitePool;

type HmacSha256 = Hmac<Sha256>;

/// Validate the admin key from the request header (constant-time comparison).
pub fn authenticate_admin(req: &actix_web::HttpRequest) -> bool {
    let config = match req.app_data::<web::Data<crate::config::Config>>() {
        Some(c) => c,
        None => return false,
    };
    let expected = match &config.admin_key {
        Some(k) if !k.is_empty() => k,
        _ => return false,
    };
    let provided = req
        .headers()
        .get("X-Admin-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided.is_empty() {
        return false;
    }
    let mut mac = HmacSha256::new_from_slice(b"admin-key-verify")
        .expect("HMAC accepts any key length");
    mac.update(expected.as_bytes());
    let expected_tag = mac.finalize().into_bytes();

    let mut mac2 = HmacSha256::new_from_slice(b"admin-key-verify")
        .expect("HMAC accepts any key length");
    mac2.update(provided.as_bytes());
    let provided_tag = mac2.finalize().into_bytes();

    expected_tag == provided_tag
}

fn unauthorized() -> actix_web::HttpResponse {
    actix_web::HttpResponse::Unauthorized().json(serde_json::json!({
        "error": "Invalid or missing admin key"
    }))
}

/// POST /api/admin/auth -- validate admin key, return success
pub async fn auth_check(req: actix_web::HttpRequest) -> actix_web::HttpResponse {
    if !authenticate_admin(&req) {
        return unauthorized();
    }
    actix_web::HttpResponse::Ok().json(serde_json::json!({ "ok": true }))
}

/// GET /api/admin/stats -- aggregate platform metrics
pub async fn stats(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    if !authenticate_admin(&req) {
        return unauthorized();
    }

    let merchant_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merchants")
        .fetch_one(pool.get_ref()).await.unwrap_or(0);

    let invoice_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices")
        .fetch_one(pool.get_ref()).await.unwrap_or(0);

    let confirmed_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE status = 'confirmed'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE status IN ('pending', 'underpaid', 'detected')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let expired_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE status = 'expired'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let draft_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE status = 'draft'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let total_zec_received: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(price_zec), 0.0) FROM invoices WHERE status = 'confirmed'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    let total_zatoshis_received: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(received_zatoshis), 0) FROM invoices WHERE status = 'confirmed'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let product_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM products WHERE active = 1"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let subscription_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM subscriptions"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let active_subscriptions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM subscriptions WHERE status = 'active'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let total_fees_collected: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(fee_amount_zec), 0.0) FROM fee_ledger WHERE auto_collected = 1 OR collected_at IS NOT NULL"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    let total_fees_outstanding: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(fee_amount_zec), 0.0) FROM fee_ledger WHERE auto_collected = 0 AND collected_at IS NULL"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    let total_fees_all: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(fee_amount_zec), 0.0) FROM fee_ledger"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    // Invoices in the last 24 hours
    let invoices_24h: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-1 day')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let confirmed_24h: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE status = 'confirmed' AND confirmed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-1 day')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let volume_24h: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(price_zec), 0.0) FROM invoices WHERE status = 'confirmed' AND confirmed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-1 day')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    // Invoices in the last 7 days
    let invoices_7d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7 days')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let confirmed_7d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE status = 'confirmed' AND confirmed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7 days')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let volume_7d: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(price_zec), 0.0) FROM invoices WHERE status = 'confirmed' AND confirmed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7 days')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    // Invoices in the last 30 days
    let invoices_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-30 days')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let confirmed_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE status = 'confirmed' AND confirmed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-30 days')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let volume_30d: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(price_zec), 0.0) FROM invoices WHERE status = 'confirmed' AND confirmed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-30 days')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    actix_web::HttpResponse::Ok().json(serde_json::json!({
        "merchants": merchant_count,
        "products": product_count,
        "invoices": {
            "total": invoice_count,
            "confirmed": confirmed_count,
            "pending": pending_count,
            "expired": expired_count,
            "draft": draft_count,
        },
        "volume": {
            "total_zec": total_zec_received,
            "total_zatoshis": total_zatoshis_received,
        },
        "fees": {
            "total": total_fees_all,
            "collected": total_fees_collected,
            "outstanding": total_fees_outstanding,
        },
        "subscriptions": {
            "total": subscription_count,
            "active": active_subscriptions,
        },
        "last_24h": {
            "invoices": invoices_24h,
            "confirmed": confirmed_24h,
            "volume_zec": volume_24h,
        },
        "last_7d": {
            "invoices": invoices_7d,
            "confirmed": confirmed_7d,
            "volume_zec": volume_7d,
        },
        "last_30d": {
            "invoices": invoices_30d,
            "confirmed": confirmed_30d,
            "volume_zec": volume_30d,
        },
    }))
}

/// GET /api/admin/merchants -- list all merchants with summary info
pub async fn merchants(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    if !authenticate_admin(&req) {
        return unauthorized();
    }

    let rows: Vec<(String, String, i64, f64, Option<String>, String, String)> = sqlx::query_as(
        "SELECT m.id, m.name,
         (SELECT COUNT(*) FROM invoices i WHERE i.merchant_id = m.id) AS invoice_count,
         (SELECT COALESCE(SUM(price_zec), 0.0) FROM invoices i WHERE i.merchant_id = m.id AND i.status = 'confirmed') AS total_zec,
         m.webhook_url,
         m.created_at,
         COALESCE(m.billing_status, 'active') AS billing_status
         FROM merchants m ORDER BY m.created_at DESC"
    )
    .fetch_all(pool.get_ref())
    .await
    .unwrap_or_default();

    let merchants: Vec<serde_json::Value> = rows.iter().map(|r| {
        serde_json::json!({
            "id": r.0,
            "name": r.1,
            "invoice_count": r.2,
            "total_zec": r.3,
            "webhook_configured": r.4.is_some() && !r.4.as_ref().unwrap().is_empty(),
            "created_at": r.5,
            "billing_status": r.6,
        })
    }).collect();

    actix_web::HttpResponse::Ok().json(merchants)
}

/// GET /api/admin/billing -- billing overview across all merchants
pub async fn billing(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    if !authenticate_admin(&req) {
        return unauthorized();
    }

    let open_cycles: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM billing_cycles WHERE status = 'open'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let invoiced_cycles: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM billing_cycles WHERE status = 'invoiced'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let past_due_cycles: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM billing_cycles WHERE status = 'past_due'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let paid_cycles: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM billing_cycles WHERE status = 'paid'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let suspended_merchants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM merchants WHERE billing_status = 'suspended'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let past_due_merchants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM merchants WHERE billing_status = 'past_due'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let total_outstanding: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(outstanding_zec), 0.0) FROM billing_cycles WHERE status IN ('open', 'invoiced', 'past_due')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    let total_collected: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(total_fees_zec), 0.0) FROM billing_cycles WHERE status = 'paid'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0.0);

    // Recent billing cycles
    let recent_cycles: Vec<(String, String, String, String, f64, f64, String, Option<String>)> = sqlx::query_as(
        "SELECT bc.id, bc.merchant_id, m.name, bc.period_end, bc.total_fees_zec, bc.outstanding_zec, bc.status, bc.grace_until
         FROM billing_cycles bc
         JOIN merchants m ON m.id = bc.merchant_id
         ORDER BY bc.created_at DESC LIMIT 20"
    )
    .fetch_all(pool.get_ref())
    .await
    .unwrap_or_default();

    let cycles_json: Vec<serde_json::Value> = recent_cycles.iter().map(|c| {
        serde_json::json!({
            "id": c.0,
            "merchant_id": c.1,
            "merchant_name": c.2,
            "period_end": c.3,
            "total_fees_zec": c.4,
            "outstanding_zec": c.5,
            "status": c.6,
            "grace_until": c.7,
        })
    }).collect();

    actix_web::HttpResponse::Ok().json(serde_json::json!({
        "cycles": {
            "open": open_cycles,
            "invoiced": invoiced_cycles,
            "past_due": past_due_cycles,
            "paid": paid_cycles,
        },
        "merchants": {
            "suspended": suspended_merchants,
            "past_due": past_due_merchants,
        },
        "totals": {
            "outstanding_zec": total_outstanding,
            "collected_zec": total_collected,
        },
        "recent_cycles": cycles_json,
    }))
}

/// GET /api/admin/system -- system health info
pub async fn system(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
    config: web::Data<crate::config::Config>,
) -> actix_web::HttpResponse {
    if !authenticate_admin(&req) {
        return unauthorized();
    }

    let scanner_height = crate::db::get_scanner_state(pool.get_ref(), "last_height").await;

    let rates = price_service.get_rates().await.ok();
    let price_info = rates.map(|r| serde_json::json!({
        "zec_eur": r.zec_eur,
        "zec_usd": r.zec_usd,
        "zec_brl": r.zec_brl,
        "zec_gbp": r.zec_gbp,
        "updated_at": r.updated_at.to_rfc3339(),
    }));

    let pending_webhooks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM webhook_deliveries WHERE status = 'pending'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let failed_webhooks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM webhook_deliveries WHERE status = 'failed'"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    let active_sessions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    ).fetch_one(pool.get_ref()).await.unwrap_or(0);

    actix_web::HttpResponse::Ok().json(serde_json::json!({
        "network": config.network,
        "scanner_height": scanner_height,
        "price_feed": price_info,
        "webhooks": {
            "pending": pending_webhooks,
            "failed": failed_webhooks,
        },
        "active_sessions": active_sessions,
        "fee_enabled": config.fee_enabled(),
        "fee_rate": config.fee_rate,
    }))
}
