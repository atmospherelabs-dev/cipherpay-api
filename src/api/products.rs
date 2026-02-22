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
        Ok(product) => HttpResponse::Created().json(product),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint") {
                HttpResponse::Conflict().json(serde_json::json!({
                    "error": "A product with this slug already exists"
                }))
            } else {
                tracing::error!(error = %e, "Failed to create product");
                HttpResponse::BadRequest().json(serde_json::json!({
                    "error": msg
                }))
            }
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
        Ok(products) => HttpResponse::Ok().json(products),
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
        Ok(Some(product)) => HttpResponse::Ok().json(product),
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

    match products::deactivate_product(pool.get_ref(), &product_id, &merchant.id).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({ "status": "deactivated" })),
        Ok(false) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Product not found"
        })),
        Err(e) => {
            tracing::error!(error = %e, "Failed to deactivate product");
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
            HttpResponse::Ok().json(serde_json::json!({
                "id": product.id,
                "name": product.name,
                "description": product.description,
                "price_eur": product.price_eur,
                "currency": product.currency,
                "variants": product.variants_list(),
                "slug": product.slug,
            }))
        }
        _ => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Product not found"
        })),
    }
}

fn validate_product_create(req: &CreateProductRequest) -> Result<(), validation::ValidationError> {
    validation::validate_length("slug", &req.slug, 100)?;
    validation::validate_length("name", &req.name, 200)?;
    if let Some(ref desc) = req.description {
        validation::validate_length("description", desc, 2000)?;
    }
    if req.price_eur < 0.0 {
        return Err(validation::ValidationError::invalid("price_eur", "must be non-negative"));
    }
    if let Some(ref variants) = req.variants {
        if variants.len() > 50 {
            return Err(validation::ValidationError::invalid("variants", "too many variants (max 50)"));
        }
        for v in variants {
            validation::validate_length("variant", v, 100)?;
        }
    }
    Ok(())
}

fn validate_product_update(req: &UpdateProductRequest) -> Result<(), validation::ValidationError> {
    if let Some(ref name) = req.name {
        validation::validate_length("name", name, 200)?;
    }
    if let Some(ref desc) = req.description {
        validation::validate_length("description", desc, 2000)?;
    }
    if let Some(price) = req.price_eur {
        if price < 0.0 {
            return Err(validation::ValidationError::invalid("price_eur", "must be non-negative"));
        }
    }
    if let Some(ref variants) = req.variants {
        if variants.len() > 50 {
            return Err(validation::ValidationError::invalid("variants", "too many variants (max 50)"));
        }
        for v in variants {
            validation::validate_length("variant", v, 100)?;
        }
    }
    Ok(())
}
