use actix_web::{web, HttpResponse};
use sqlx::SqlitePool;

use crate::invoices;

pub async fn get(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();

    match invoices::get_invoice_status(pool.get_ref(), &id).await {
        Ok(Some(status)) => HttpResponse::Ok().json(status),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Invoice not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get invoice status");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}
