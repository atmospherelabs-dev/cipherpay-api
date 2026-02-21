use actix_web::{web, HttpResponse};

use crate::invoices::pricing::PriceService;

pub async fn get(price_service: web::Data<PriceService>) -> HttpResponse {
    match price_service.get_rates().await {
        Ok(rates) => HttpResponse::Ok().json(rates),
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch rates");
            HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Price feed unavailable"
            }))
        }
    }
}
