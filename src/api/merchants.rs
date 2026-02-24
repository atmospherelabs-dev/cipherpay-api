use actix_web::{web, HttpResponse};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::merchants::{CreateMerchantRequest, create_merchant};
use crate::validation;

pub async fn create(
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<CreateMerchantRequest>,
) -> HttpResponse {
    if let Err(e) = validate_registration(&body, config.is_testnet()) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    match create_merchant(pool.get_ref(), &body, &config.encryption_key).await {
        Ok(resp) => HttpResponse::Created().json(resp),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create merchant");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create merchant"
            }))
        }
    }
}

fn validate_registration(
    req: &CreateMerchantRequest,
    is_testnet: bool,
) -> Result<(), validation::ValidationError> {
    if let Some(ref name) = req.name {
        validation::validate_length("name", name, 100)?;
    }
    validation::validate_length("ufvk", &req.ufvk, 2000)?;
    validation::validate_ufvk_network("ufvk", &req.ufvk, is_testnet)?;
    if let Some(ref url) = req.webhook_url {
        if !url.is_empty() {
            validation::validate_webhook_url("webhook_url", url, is_testnet)?;
        }
    }
    if let Some(ref email) = req.email {
        if !email.is_empty() {
            validation::validate_email_format("email", email)?;
        }
    }
    Ok(())
}
