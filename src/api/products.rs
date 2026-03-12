use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::products::{self, CreateProductRequest, UpdateProductRequest};
use crate::validation;

pub async fn create(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    body: web::Json<CreateProductRequest>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    if let Err(e) = validate_product_create(&body) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    match products::create_product(pool.get_ref(), &merchant.id, &body).await {
        Ok(product) => {
            let currency = body.currency.as_deref().unwrap_or("EUR").to_uppercase();
            let price = match crate::prices::create_price(
                pool.get_ref(),
                &merchant.id,
                &crate::prices::CreatePriceRequest {
                    product_id: product.id.clone(),
                    currency: currency.clone(),
                    unit_amount: body.unit_amount,
                    price_type: body.price_type.clone(),
                    billing_interval: body.billing_interval.clone(),
                    interval_count: body.interval_count,
                },
            ).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create default price");
                    return HttpResponse::BadRequest().json(serde_json::json!({
                        "error": e.to_string()
                    }));
                }
            };

            if let Err(e) = products::set_default_price(pool.get_ref(), &product.id, &price.id).await {
                tracing::error!(error = %e, "Failed to set default price on product");
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to set default price"
                }));
            }

            let product = match products::get_product(pool.get_ref(), &product.id).await {
                Ok(Some(p)) => p,
                _ => return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Product not found after price creation"
                })),
            };

            let prices = crate::prices::list_prices_for_product(pool.get_ref(), &product.id)
                .await.unwrap_or_default();

            HttpResponse::Created().json(serde_json::json!({
                "id": product.id,
                "merchant_id": product.merchant_id,
                "slug": product.slug,
                "name": product.name,
                "description": product.description,
                "default_price_id": product.default_price_id,
                "metadata": product.metadata_json(),
                "active": product.active,
                "created_at": product.created_at,
                "prices": prices,
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to create product");
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": e.to_string()
            }))
        }
    }
}

pub async fn list(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
) -> HttpResponse {
    let merchant = match super::auth::resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Not authenticated"
            }));
        }
    };

    match products::list_products(pool.get_ref(), &merchant.id).await {
        Ok(product_list) => {
            let mut result = Vec::new();
            for product in &product_list {
                let prices = crate::prices::list_prices_for_product(pool.get_ref(), &product.id)
                    .await.unwrap_or_default();
                result.push(serde_json::json!({
                    "id": product.id,
                    "merchant_id": product.merchant_id,
                    "slug": product.slug,
                    "name": product.name,
                    "description": product.description,
                    "default_price_id": product.default_price_id,
                    "metadata": product.metadata_json(),
                    "active": product.active,
                    "created_at": product.created_at,
                    "prices": prices,
                }));
            }
            HttpResponse::Ok().json(result)
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list products");
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
    body: web::Json<UpdateProductRequest>,
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

    if let Err(e) = validate_product_update(&body) {
        return HttpResponse::BadRequest().json(e.to_json());
    }

    match products::update_product(pool.get_ref(), &product_id, &merchant.id, &body).await {
        Ok(Some(product)) => {
            let prices = crate::prices::list_prices_for_product(pool.get_ref(), &product.id)
                .await.unwrap_or_default();

            HttpResponse::Ok().json(serde_json::json!({
                "id": product.id,
                "merchant_id": product.merchant_id,
                "slug": product.slug,
                "name": product.name,
                "description": product.description,
                "default_price_id": product.default_price_id,
                "metadata": product.metadata_json(),
                "active": product.active,
                "created_at": product.created_at,
                "prices": prices,
            }))
        }
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Product not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update product");
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

    let product_id = path.into_inner();

    match products::delete_product(pool.get_ref(), &product_id, &merchant.id).await {
        Ok(products::DeleteOutcome::Deleted) => {
            HttpResponse::Ok().json(serde_json::json!({ "status": "deleted" }))
        }
        Ok(products::DeleteOutcome::Archived) => {
            HttpResponse::Ok().json(serde_json::json!({ "status": "archived" }))
        }
        Ok(products::DeleteOutcome::NotFound) => {
            HttpResponse::NotFound().json(serde_json::json!({
                "error": "Product not found"
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete product");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal error"
            }))
        }
    }
}

/// Public endpoint: get product details for buyers (only active products)
pub async fn get_public(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let product_id = path.into_inner();

    match products::get_product(pool.get_ref(), &product_id).await {
        Ok(Some(product)) if product.active == 1 => {
            let prices = crate::prices::list_prices_for_product(pool.get_ref(), &product.id)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.active == 1)
                .collect::<Vec<_>>();

            HttpResponse::Ok().json(serde_json::json!({
                "id": product.id,
                "name": product.name,
                "description": product.description,
                "default_price_id": product.default_price_id,
                "metadata": product.metadata_json(),
                "slug": product.slug,
                "prices": prices,
            }))
        }
        _ => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Product not found"
        })),
    }
}

fn validate_product_create(req: &CreateProductRequest) -> Result<(), validation::ValidationError> {
    if let Some(ref slug) = req.slug {
        validation::validate_length("slug", slug, 100)?;
    }
    validation::validate_length("name", &req.name, 200)?;
    if let Some(ref desc) = req.description {
        validation::validate_length("description", desc, 2000)?;
    }
    if req.unit_amount <= 0.0 {
        return Err(validation::ValidationError::invalid("unit_amount", "must be greater than 0"));
    }
    if req.unit_amount > 1_000_000.0 {
        return Err(validation::ValidationError::invalid("unit_amount", "exceeds maximum of 1000000"));
    }
    Ok(())
}

fn validate_product_update(req: &UpdateProductRequest) -> Result<(), validation::ValidationError> {
    if let Some(ref name) = req.name {
        if name.is_empty() {
            return Err(validation::ValidationError::invalid("name", "must not be empty"));
        }
        validation::validate_length("name", name, 200)?;
    }
    if let Some(ref desc) = req.description {
        validation::validate_length("description", desc, 2000)?;
    }
    Ok(())
}
