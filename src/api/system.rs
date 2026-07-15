use actix_web::{web, HttpResponse};
use sqlx::SqlitePool;

pub async fn health(
    pool: web::Data<SqlitePool>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
) -> HttpResponse {
    let mut checks = serde_json::Map::new();
    let mut degraded = false;
    let mut unhealthy = false;

    // 1. Database: test that invoice queries work (catches column mismatch)
    let db_ok = match sqlx::query_as::<_, crate::invoices::Invoice>(
        "SELECT i.id, i.merchant_id, i.memo_code, i.product_id, i.product_name, i.size,
         i.price_eur, i.price_usd, i.currency, i.price_zec, i.zec_rate_at_creation,
         i.amount, i.price_id,
         i.payment_address,
         i.zcash_uri,
         NULL AS merchant_name,
         i.refund_address, i.status, i.detected_txid, i.detected_at,
         i.confirmed_at, i.refunded_at, i.refund_txid, i.expires_at, i.purge_after, i.created_at,
         i.orchard_receiver_hex, i.diversifier_index,
         i.price_zatoshis, i.received_zatoshis,
         i.subscription_id,
         i.payment_link_id, i.is_donation, i.campaign_counted,
         i.confirmed_rate, i.confirmed_fiat_amount
         FROM invoices i LIMIT 1",
    )
    .fetch_optional(pool.get_ref())
    .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::error!(error = %e, "Health check: invoice query failed");
            false
        }
    };
    if !db_ok {
        unhealthy = true;
    }
    checks.insert("database".into(), serde_json::json!(if db_ok { "ok" } else { "error" }));

    // 2. Scanner: check metrics for errors and staleness
    let m = crate::scanner::metrics::global();
    let snap = m.snapshot();
    let scanner_status = snap.status();
    let recent_errors = m.recent_errors().await;
    let blocks_behind = snap.blocks_behind();
    let last_error = m.last_error().await;

    if scanner_status == "behind" || (recent_errors > 0 && snap.last_block_height == 0) {
        unhealthy = true;
    } else if scanner_status == "catching_up" || blocks_behind > 5 {
        degraded = true;
    }
    let mut scanner_json = serde_json::json!({
        "status": scanner_status,
        "blocks_behind": blocks_behind,
        "scan_errors": recent_errors,
        "last_block_height": snap.last_block_height,
    });
    if let Some((msg, ago_secs)) = last_error {
        scanner_json["last_error"] = serde_json::json!(msg);
        scanner_json["last_error_ago_secs"] = serde_json::json!(ago_secs);
    }
    checks.insert("scanner".into(), scanner_json);

    // 3. Price feed: check if rates are available and fresh
    let price_ok = match price_service.get_rates().await {
        Ok(rates) => {
            let age_secs = (chrono::Utc::now() - rates.updated_at).num_seconds();
            if age_secs > 600 {
                degraded = true;
                checks.insert("price_feed".into(), serde_json::json!({
                    "status": "stale",
                    "age_secs": age_secs,
                }));
                false
            } else {
                checks.insert("price_feed".into(), serde_json::json!("ok"));
                true
            }
        }
        Err(_) => {
            degraded = true;
            checks.insert("price_feed".into(), serde_json::json!("unavailable"));
            false
        }
    };
    let _ = price_ok;

    let status = if unhealthy {
        "unhealthy"
    } else if degraded {
        "degraded"
    } else {
        "ok"
    };

    let body = serde_json::json!({
        "status": status,
        "service": "cipherpay",
        "checks": checks,
    });

    if unhealthy {
        HttpResponse::ServiceUnavailable().json(body)
    } else {
        HttpResponse::Ok().json(body)
    }
}

pub async fn well_known_payment(config: web::Data<crate::config::Config>) -> HttpResponse {
    let network = if config.is_testnet() {
        "zcash:testnet"
    } else {
        "zcash:mainnet"
    };
    let base_url = if config.is_testnet() {
        "https://testnet.api.cipherpay.app"
    } else {
        "https://api.cipherpay.app"
    };
    HttpResponse::Ok()
        .insert_header(("Access-Control-Allow-Origin", "*"))
        .insert_header(("Cache-Control", "public, max-age=3600"))
        .json(serde_json::json!({
            "version": "1.0",
            "x402Version": 2,
            "methods": ["zcash"],
            "currencies": ["ZEC"],
            "network": network,
            "asset": "ZEC",
            "scheme": "exact",
            "protocols": ["x402", "mpp"],
            "capabilities": {
                "sessions": true,
                "streaming": true,
                "replay_protection": true,
                "idempotency": true,
                "auto_session": true,
            },
            "facilitator": base_url,
            "facilitatorUrl": format!("{}/api/x402", base_url),
            "endpoints": {
                "verify": format!("{}/api/x402/v2/verify", base_url),
                "settle": format!("{}/api/x402/v2/settle", base_url),
                "supported": format!("{}/api/x402/supported", base_url),
                "session_prepare": format!("{}/api/sessions/prepare", base_url),
                "session_validate": format!("{}/api/sessions/validate", base_url),
            },
            "documentation": "https://cipherpay.app/docs",
        }))
}
