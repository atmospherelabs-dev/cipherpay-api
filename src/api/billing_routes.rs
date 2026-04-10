use actix_web::web;
use sqlx::SqlitePool;

use super::auth;

pub async fn billing_summary(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    if !config.fee_enabled() {
        return actix_web::HttpResponse::Ok().json(serde_json::json!({
            "fee_enabled": false,
            "fee_rate": 0.0,
            "billing_status": "active",
            "trust_tier": "standard",
        }));
    }

    match crate::billing::get_billing_summary(pool.get_ref(), &merchant.id, &config).await {
        Ok(summary) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "fee_enabled": true,
            "fee_rate": summary.fee_rate,
            "trust_tier": summary.trust_tier,
            "billing_status": summary.billing_status,
            "current_cycle": summary.current_cycle,
            "total_fees_zec": summary.total_fees_zec,
            "auto_collected_zec": summary.auto_collected_zec,
            "outstanding_zec": summary.outstanding_zec,
            "min_settlement_zec": 0.05,
            "settlement_invoice_status": summary.settlement_invoice_status,
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get billing summary");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn billing_history(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    match crate::billing::get_billing_history(pool.get_ref(), &merchant.id).await {
        Ok(cycles) => actix_web::HttpResponse::Ok().json(cycles),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get billing history");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

pub async fn billing_settle(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    price_service: web::Data<crate::invoices::pricing::PriceService>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    let fee_address = match &config.fee_address {
        Some(addr) => addr.clone(),
        None => {
            return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Billing not enabled"
            }));
        }
    };

    let summary =
        match crate::billing::get_billing_summary(pool.get_ref(), &merchant.id, &config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "Failed to get billing for settle");
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }));
            }
        };

    if summary.outstanding_zatoshis <= 0 {
        return actix_web::HttpResponse::Ok().json(serde_json::json!({
            "message": "No outstanding balance",
            "outstanding_zec": 0.0,
        }));
    }

    const MIN_SETTLEMENT_ZATOSHIS: i64 = 5_000_000;
    const MIN_SETTLEMENT_ZEC: f64 = 0.05;
    if summary.outstanding_zatoshis < MIN_SETTLEMENT_ZATOSHIS {
        return actix_web::HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Outstanding balance ({:.6} ZEC) is below the minimum settlement amount ({:.2} ZEC). Fees will carry over until the threshold is reached.", summary.outstanding_zec, MIN_SETTLEMENT_ZEC),
            "outstanding_zec": summary.outstanding_zec,
            "min_settlement_zec": MIN_SETTLEMENT_ZEC,
        }));
    }

    let rates = match price_service.get_rates().await {
        Ok(r) => r,
        Err(_) => crate::invoices::pricing::ZecRates {
            zec_eur: 0.0,
            zec_usd: 0.0,
            zec_brl: 0.0,
            zec_gbp: 0.0,
            zec_cad: 0.0,
            zec_jpy: 0.0,
            zec_mxn: 0.0,
            zec_ars: 0.0,
            zec_ngn: 0.0,
            zec_chf: 0.0,
            zec_inr: 0.0,
            updated_at: chrono::Utc::now(),
        },
    };

    match crate::billing::create_settlement_invoice(
        pool.get_ref(),
        &merchant.id,
        summary.outstanding_zatoshis,
        &fee_address,
        rates.zec_eur,
        rates.zec_usd,
    )
    .await
    {
        Ok(invoice_id) => {
            if let Some(cycle) = &summary.current_cycle {
                let _ = sqlx::query(
                    "UPDATE billing_cycles SET settlement_invoice_id = ?, status = 'invoiced',
                     grace_until = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '+7 days')
                     WHERE id = ? AND status = 'open'",
                )
                .bind(&invoice_id)
                .bind(&cycle.id)
                .execute(pool.get_ref())
                .await;
            }

            actix_web::HttpResponse::Created().json(serde_json::json!({
                "invoice_id": invoice_id,
                "outstanding_zec": summary.outstanding_zec,
                "message": "Settlement invoice created. Pay to restore full access.",
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to create settlement invoice");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create settlement invoice"
            }))
        }
    }
}

pub async fn delete_account(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
) -> actix_web::HttpResponse {
    let merchant = match auth::require_session(&req, pool.get_ref()).await {
        Ok(merchant) => merchant,
        Err(response) => return response,
    };

    if config.fee_enabled() {
        match crate::merchants::has_outstanding_balance(pool.get_ref(), &merchant.id).await {
            Ok(true) => {
                return actix_web::HttpResponse::Forbidden().json(serde_json::json!({
                    "error": "Cannot delete account with outstanding billing balance. Please settle your fees first."
                }));
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to check billing balance");
                return actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Internal error"
                }));
            }
            _ => {}
        }
    }

    match crate::merchants::delete_merchant(pool.get_ref(), &merchant.id).await {
        Ok(()) => actix_web::HttpResponse::Ok().json(serde_json::json!({
            "status": "deleted",
            "message": "Your account and all associated data have been permanently deleted."
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete merchant account");
            actix_web::HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to delete account"
            }))
        }
    }
}
