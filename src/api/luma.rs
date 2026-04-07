use actix_web::{web, HttpRequest, HttpResponse};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::events::parse_event_datetime;

use super::auth::resolve_session;

/// Normalize a Luma ISO 8601 datetime (e.g. "2026-04-01T15:00:00.000Z") to "YYYY-MM-DDTHH:MM"
fn normalize_luma_date(s: &str) -> Option<String> {
    parse_event_datetime(s).map(|dt| dt.format("%Y-%m-%dT%H:%M").to_string())
}

/// GET /api/luma/events -- list importable Luma events for the authenticated merchant
pub async fn list_events(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({"error": "Not authenticated"}))
        }
    };

    let api_key = match get_luma_key(pool.get_ref(), &merchant.id, &config.encryption_key).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "Luma API key not configured"}))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to decrypt Luma API key");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let http = reqwest::Client::new();

    let events = match crate::luma::list_events(&http, &api_key).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "Failed to list Luma events");
            return HttpResponse::BadGateway()
                .json(serde_json::json!({"error": format!("Luma API error: {}", e)}));
        }
    };

    let already_imported: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT luma_event_id FROM events WHERE merchant_id = ? AND luma_event_id IS NOT NULL",
    )
    .bind(&merchant.id)
    .fetch_all(pool.get_ref())
    .await
    .unwrap_or_default()
    .into_iter()
    .collect();

    let now = chrono::Utc::now().naive_utc();

    let mut result = Vec::new();
    for ev in events {
        if already_imported.contains(&ev.api_id) {
            continue;
        }

        // Skip past events
        if let Some(ref start) = ev.start_at {
            if let Some(dt) = parse_event_datetime(start) {
                if dt < now {
                    continue;
                }
            }
        }

        let tiers = match crate::luma::list_ticket_types(&http, &api_key, &ev.api_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(event_id = %ev.api_id, error = %e, "Failed to list ticket types for Luma event, skipping");
                Vec::new()
            }
        };

        result.push(serde_json::json!({
            "api_id": ev.api_id,
            "name": ev.name,
            "start_at": ev.start_at,
            "end_at": ev.end_at,
            "cover_url": ev.cover_url,
            "url": ev.url,
            "timezone": ev.timezone,
            "geo_address_json": ev.geo_address_json,
            "ticket_types": tiers,
        }));
    }

    HttpResponse::Ok().json(result)
}

/// POST /api/luma/import -- import a Luma event as a CipherPay product + event
pub async fn import_event(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    body: web::Json<ImportRequest>,
) -> HttpResponse {
    let merchant = match resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({"error": "Not authenticated"}))
        }
    };

    let api_key = match get_luma_key(pool.get_ref(), &merchant.id, &config.encryption_key).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "Luma API key not configured"}))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to decrypt Luma API key");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM events WHERE merchant_id = ? AND luma_event_id = ?")
            .bind(&merchant.id)
            .bind(&body.luma_event_id)
            .fetch_optional(pool.get_ref())
            .await
            .unwrap_or(None);

    if existing.is_some() {
        return HttpResponse::Conflict()
            .json(serde_json::json!({"error": "Event already imported"}));
    }

    let http = reqwest::Client::new();

    let events = match crate::luma::list_events(&http, &api_key).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch Luma events for import");
            return HttpResponse::BadGateway()
                .json(serde_json::json!({"error": format!("Luma API error: {}", e)}));
        }
    };

    let luma_event = match events.into_iter().find(|e| e.api_id == body.luma_event_id) {
        Some(e) => e,
        None => {
            return HttpResponse::NotFound()
                .json(serde_json::json!({"error": "Luma event not found"}))
        }
    };

    let ticket_types = crate::luma::list_ticket_types(&http, &api_key, &body.luma_event_id)
        .await
        .unwrap_or_default();

    if ticket_types.is_empty() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "No ticket types found for this Luma event"
        }));
    }

    let location = luma_event
        .geo_address_json
        .as_ref()
        .and_then(|g| g.get("full_address"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let luma_url = luma_event.url.clone();

    let prices: Vec<crate::events::CreateEventPrice> = ticket_types
        .iter()
        .map(|tt| {
            let (currency, amount) = tt
                .price
                .as_ref()
                .map(|p| {
                    let c = p
                        .currency
                        .clone()
                        .unwrap_or_else(|| "USD".into())
                        .to_uppercase();
                    let a = p.amount.unwrap_or(0.0);
                    (c, a)
                })
                .unwrap_or_else(|| ("USD".into(), 0.0));

            crate::events::CreateEventPrice {
                currency,
                unit_amount: if amount > 0.0 { amount } else { 0.01 },
                label: tt.name.clone(),
                max_quantity: tt.max_capacity,
            }
        })
        .collect();

    let event_date = luma_event.start_at.as_deref().and_then(normalize_luma_date);

    let create_req = crate::events::CreateEventRequest {
        title: luma_event.name.clone(),
        description: None,
        event_date,
        event_location: location,
        prices,
    };

    let event = match crate::events::create_event_with_product_and_prices(
        pool.get_ref(),
        &merchant.id,
        &create_req,
    )
    .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create event from Luma import");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": format!("Failed to create event: {}", e)}));
        }
    };

    // Set luma_event_id and luma_event_url on the event row
    sqlx::query("UPDATE events SET luma_event_id = ?, luma_event_url = ? WHERE id = ?")
        .bind(&body.luma_event_id)
        .bind(&luma_url)
        .bind(&event.id)
        .execute(pool.get_ref())
        .await
        .ok();

    // Set luma_ticket_type_id on each price
    let price_rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, label FROM prices WHERE product_id = ? AND active = 1 ORDER BY unit_amount ASC",
    )
    .bind(&event.product_id)
    .fetch_all(pool.get_ref())
    .await
    .unwrap_or_default();

    for (price_id, label) in &price_rows {
        if let Some(tt) = ticket_types
            .iter()
            .find(|tt| tt.name.as_deref() == label.as_deref())
        {
            sqlx::query("UPDATE prices SET luma_ticket_type_id = ? WHERE id = ?")
                .bind(&tt.api_id)
                .bind(price_id)
                .execute(pool.get_ref())
                .await
                .ok();
        }
    }

    HttpResponse::Ok().json(serde_json::json!({
        "event_id": event.id,
        "product_id": event.product_id,
        "title": event.title,
        "luma_event_id": body.luma_event_id,
    }))
}

/// GET /api/invoices/{id}/luma-pass -- public endpoint for confirmation page polling
pub async fn luma_pass(pool: web::Data<SqlitePool>, path: web::Path<String>) -> HttpResponse {
    let invoice_id = path.into_inner();

    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT luma_registration_status, luma_guest_data FROM invoices WHERE id = ?",
    )
    .bind(&invoice_id)
    .fetch_optional(pool.get_ref())
    .await
    .unwrap_or(None);

    let (status, guest_data) = match row {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound().json(serde_json::json!({"error": "Invoice not found"}))
        }
    };

    let status = status.unwrap_or_else(|| "none".into());

    if status == "none" {
        return HttpResponse::Ok().json(serde_json::json!({
            "status": "not_luma",
        }));
    }

    // Map "retry" to "pending" for the frontend — the buyer sees it as still processing
    let public_status = if status == "retry" {
        "pending".to_string()
    } else {
        status
    };

    let guest: Option<serde_json::Value> = guest_data.and_then(|d| serde_json::from_str(&d).ok());

    // Also fetch event metadata for display
    let event_meta: Option<(String, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT e.title, e.event_date, e.event_location, e.luma_event_url
         FROM invoices i
         JOIN events e ON e.product_id = i.product_id
         WHERE i.id = ?",
        )
        .bind(&invoice_id)
        .fetch_optional(pool.get_ref())
        .await
        .unwrap_or(None);

    let price_label: Option<String> = sqlx::query_scalar(
        "SELECT p.label FROM invoices i JOIN prices p ON p.id = i.price_id WHERE i.id = ?",
    )
    .bind(&invoice_id)
    .fetch_optional(pool.get_ref())
    .await
    .unwrap_or(None);

    HttpResponse::Ok().json(serde_json::json!({
        "status": public_status,
        "guest": guest,
        "event_title": event_meta.as_ref().map(|e| &e.0),
        "event_date": event_meta.as_ref().and_then(|e| e.1.as_ref()),
        "event_location": event_meta.as_ref().and_then(|e| e.2.as_ref()),
        "luma_event_url": event_meta.as_ref().and_then(|e| e.3.as_ref()),
        "ticket_type": price_label,
    }))
}

#[derive(serde::Deserialize)]
pub struct ImportRequest {
    pub luma_event_id: String,
}

/// POST /api/luma/sync/{event_id} -- re-fetch Luma event data and update metadata + tiers
pub async fn sync_event(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<Config>,
    path: web::Path<String>,
) -> HttpResponse {
    let event_id = path.into_inner();

    let merchant = match resolve_session(&req, &pool).await {
        Some(m) => m,
        None => {
            return HttpResponse::Unauthorized()
                .json(serde_json::json!({"error": "Not authenticated"}))
        }
    };

    let api_key = match get_luma_key(pool.get_ref(), &merchant.id, &config.encryption_key).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "Luma API key not configured"}))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to decrypt Luma API key");
            return HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Internal error"}));
        }
    };

    // Look up event and verify ownership + Luma link
    let row: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT id, luma_event_id, product_id FROM events WHERE id = ? AND merchant_id = ?",
    )
    .bind(&event_id)
    .bind(&merchant.id)
    .fetch_optional(pool.get_ref())
    .await
    .unwrap_or(None);

    let (_, luma_event_id, product_id) = match row {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound().json(serde_json::json!({"error": "Event not found"}))
        }
    };

    let luma_event_id = match luma_event_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            return HttpResponse::BadRequest()
                .json(serde_json::json!({"error": "Event is not linked to Luma"}))
        }
    };

    let http = reqwest::Client::new();

    // Fetch latest event data from Luma
    let events = match crate::luma::list_events(&http, &api_key).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "Luma sync: failed to fetch events");
            return HttpResponse::BadGateway()
                .json(serde_json::json!({"error": format!("Luma API error: {}", e)}));
        }
    };

    let luma_event = events.into_iter().find(|e| e.api_id == luma_event_id);

    // Event gone from Luma (cancelled/deleted) → auto-cancel on CipherPay
    let luma_event = match luma_event {
        Some(e) => e,
        None => {
            tracing::info!(event_id = %event_id, "Luma event no longer exists, cancelling locally");
            match crate::events::archive_event(pool.get_ref(), &merchant.id, &event_id).await {
                Ok(_) => {
                    return HttpResponse::Ok().json(serde_json::json!({
                        "cancelled": true,
                        "reason": "Event no longer exists on Luma",
                    }))
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to cancel event after Luma removal");
                    return HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "Failed to cancel event"}));
                }
            }
        }
    };

    // Event date has passed → transition to 'past' status
    let now = chrono::Utc::now().naive_utc();
    if let Some(ref start) = luma_event.start_at {
        if let Some(dt) = parse_event_datetime(start) {
            if dt < now {
                tracing::info!(event_id = %event_id, "Luma event date has passed, marking as past");
                sqlx::query(
                    "UPDATE events SET status = 'past' WHERE id = ? AND status != 'cancelled'",
                )
                .bind(&event_id)
                .execute(pool.get_ref())
                .await
                .ok();
                sqlx::query("UPDATE products SET active = 0 WHERE id = ?")
                    .bind(&product_id)
                    .execute(pool.get_ref())
                    .await
                    .ok();
                return HttpResponse::Ok().json(serde_json::json!({
                    "past": true,
                    "reason": "Event date has passed",
                }));
            }
        }
    }

    let ticket_types = crate::luma::list_ticket_types(&http, &api_key, &luma_event_id)
        .await
        .unwrap_or_default();

    // Update event metadata
    let location = luma_event
        .geo_address_json
        .as_ref()
        .and_then(|g| g.get("full_address"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let synced_date = luma_event.start_at.as_deref().and_then(normalize_luma_date);

    sqlx::query(
        "UPDATE events SET title = ?, event_date = ?, event_location = ?, luma_event_url = ? WHERE id = ?",
    )
    .bind(&luma_event.name)
    .bind(&synced_date)
    .bind(&location)
    .bind(&luma_event.url)
    .bind(&event_id)
    .execute(pool.get_ref())
    .await
    .ok();

    // Also update the product name to match
    sqlx::query("UPDATE products SET name = ? WHERE id = ?")
        .bind(&luma_event.name)
        .bind(&product_id)
        .execute(pool.get_ref())
        .await
        .ok();

    // Sync ticket tiers: update existing, add new ones
    let existing_prices: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, label, luma_ticket_type_id FROM prices WHERE product_id = ? AND active = 1",
    )
    .bind(&product_id)
    .fetch_all(pool.get_ref())
    .await
    .unwrap_or_default();

    let mut synced_count: u32 = 0;
    let mut added_count: u32 = 0;

    for tt in &ticket_types {
        let (currency, amount) = tt
            .price
            .as_ref()
            .map(|p| {
                let c = p
                    .currency
                    .clone()
                    .unwrap_or_else(|| "USD".into())
                    .to_uppercase();
                let a = p.amount.unwrap_or(0.0);
                (c, a)
            })
            .unwrap_or_else(|| ("USD".into(), 0.0));

        let unit_amount = if amount > 0.0 { amount } else { 0.01 };

        // Try to match by luma_ticket_type_id first, then by label
        let matched = existing_prices
            .iter()
            .find(|(_, _, luma_id)| luma_id.as_deref() == Some(&tt.api_id))
            .or_else(|| {
                existing_prices
                    .iter()
                    .find(|(_, label, _)| label.as_deref() == tt.name.as_deref())
            });

        if let Some((price_id, _, _)) = matched {
            sqlx::query(
                "UPDATE prices SET unit_amount = ?, max_quantity = ?, currency = ?, label = ?, luma_ticket_type_id = ? WHERE id = ?",
            )
            .bind(unit_amount)
            .bind(tt.max_capacity)
            .bind(&currency)
            .bind(&tt.name)
            .bind(&tt.api_id)
            .bind(price_id)
            .execute(pool.get_ref())
            .await
            .ok();
            synced_count += 1;
        } else {
            // New tier on Luma that doesn't exist locally — create it
            let price_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO prices (id, product_id, currency, unit_amount, label, max_quantity, luma_ticket_type_id, active, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?)",
            )
            .bind(&price_id)
            .bind(&product_id)
            .bind(&currency)
            .bind(unit_amount)
            .bind(&tt.name)
            .bind(tt.max_capacity)
            .bind(&tt.api_id)
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(pool.get_ref())
            .await
            .ok();
            added_count += 1;
        }
    }

    // Deactivate local tiers whose luma_ticket_type_id no longer exists on Luma
    let luma_ids: Vec<&str> = ticket_types.iter().map(|tt| tt.api_id.as_str()).collect();
    let mut deactivated_count: u32 = 0;
    for (price_id, _, luma_id) in &existing_prices {
        if let Some(lid) = luma_id {
            if !luma_ids.contains(&lid.as_str()) {
                sqlx::query("UPDATE prices SET active = 0 WHERE id = ?")
                    .bind(price_id)
                    .execute(pool.get_ref())
                    .await
                    .ok();
                deactivated_count += 1;
            }
        }
    }

    HttpResponse::Ok().json(serde_json::json!({
        "synced": synced_count,
        "added": added_count,
        "deactivated": deactivated_count,
        "title": luma_event.name,
    }))
}

async fn get_luma_key(
    pool: &SqlitePool,
    merchant_id: &str,
    encryption_key: &str,
) -> anyhow::Result<Option<String>> {
    let raw: Option<String> = sqlx::query_scalar("SELECT luma_api_key FROM merchants WHERE id = ?")
        .bind(merchant_id)
        .fetch_optional(pool)
        .await?
        .flatten();

    match raw {
        Some(encrypted) if !encrypted.is_empty() => {
            if encryption_key.is_empty() {
                Ok(Some(encrypted))
            } else {
                Ok(Some(crate::crypto::decrypt(&encrypted, encryption_key)?))
            }
        }
        _ => Ok(None),
    }
}
