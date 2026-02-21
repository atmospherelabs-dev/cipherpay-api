use actix_web::{web, HttpResponse};
use sqlx::SqlitePool;

use crate::merchants::{CreateMerchantRequest, create_merchant};

pub async fn create(
    pool: web::Data<SqlitePool>,
    body: web::Json<CreateMerchantRequest>,
) -> HttpResponse {
    match create_merchant(pool.get_ref(), &body).await {
        Ok(resp) => HttpResponse::Created().json(resp),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create merchant");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create merchant"
            }))
        }
    }
}
