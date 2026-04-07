use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

const LUMA_BASE: &str = "https://public-api.luma.com/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LumaEvent {
    pub api_id: String,
    pub name: String,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub cover_url: Option<String>,
    pub url: Option<String>,
    pub timezone: Option<String>,
    pub geo_address_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LumaTicketTypePrice {
    pub amount: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LumaTicketType {
    pub api_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub price: Option<LumaTicketTypePrice>,
    pub max_capacity: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LumaGuest {
    pub api_id: Option<String>,
    pub approval_status: Option<String>,
    pub check_in_qr_code: Option<String>,
    pub event_ticket: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddGuestResponse {
    pub api_id: Option<String>,
    pub approval_status: Option<String>,
}

// -- Luma API response wrappers (their JSON shape) --

#[derive(Deserialize)]
struct LumaCalendarEntry {
    event: Option<LumaRawEvent>,
}

#[derive(Deserialize)]
struct LumaRawEvent {
    api_id: Option<String>,
    name: Option<String>,
    start_at: Option<String>,
    end_at: Option<String>,
    cover_url: Option<String>,
    url: Option<String>,
    timezone: Option<String>,
    geo_address_json: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ListEventsResponse {
    entries: Option<Vec<LumaCalendarEntry>>,
}

#[derive(Deserialize)]
struct LumaRawTicketType {
    api_id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    price: Option<LumaTicketTypePrice>,
    max_capacity: Option<i64>,
}

#[derive(Deserialize)]
struct ListTicketTypesResponse {
    ticket_types: Option<Vec<LumaRawTicketType>>,
}

#[derive(Deserialize)]
struct AddGuestsResponseWrapper {
    entries: Option<Vec<AddGuestEntry>>,
}

#[derive(Deserialize)]
struct AddGuestEntry {
    guest: Option<AddGuestRaw>,
}

#[derive(Deserialize)]
struct AddGuestRaw {
    api_id: Option<String>,
    approval_status: Option<String>,
}

#[derive(Deserialize)]
struct GetGuestResponseWrapper {
    entries: Option<Vec<GetGuestEntry>>,
}

#[derive(Deserialize)]
struct GetGuestEntry {
    guest: Option<LumaGuestRaw>,
}

#[derive(Deserialize)]
struct LumaGuestRaw {
    api_id: Option<String>,
    approval_status: Option<String>,
    check_in_qr_code: Option<String>,
    event_ticket: Option<serde_json::Value>,
}

pub async fn list_events(http: &reqwest::Client, api_key: &str) -> Result<Vec<LumaEvent>> {
    let resp = http
        .get(format!("{}/calendar/list-events", LUMA_BASE))
        .header("x-luma-api-key", api_key)
        .send()
        .await
        .map_err(|e| anyhow!("Luma API request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Luma list-events failed ({}): {}", status, body));
    }

    let data: ListEventsResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("Failed to parse Luma events response: {}", e))?;

    let events = data
        .entries
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let ev = entry.event?;
            Some(LumaEvent {
                api_id: ev.api_id?,
                name: ev.name.unwrap_or_default(),
                start_at: ev.start_at,
                end_at: ev.end_at,
                cover_url: ev.cover_url,
                url: ev.url,
                timezone: ev.timezone,
                geo_address_json: ev.geo_address_json,
            })
        })
        .collect();

    Ok(events)
}

pub async fn list_ticket_types(
    http: &reqwest::Client,
    api_key: &str,
    event_api_id: &str,
) -> Result<Vec<LumaTicketType>> {
    let resp = http
        .get(format!("{}/event/ticket-types/list", LUMA_BASE))
        .header("x-luma-api-key", api_key)
        .query(&[("event_api_id", event_api_id)])
        .send()
        .await
        .map_err(|e| anyhow!("Luma ticket-types request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Luma list-ticket-types failed ({}): {}",
            status,
            body
        ));
    }

    let data: ListTicketTypesResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("Failed to parse Luma ticket types: {}", e))?;

    let types = data
        .ticket_types
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tt| {
            Some(LumaTicketType {
                api_id: tt.api_id?,
                name: tt.name,
                description: tt.description,
                price: tt.price,
                max_capacity: tt.max_capacity,
            })
        })
        .collect();

    Ok(types)
}

pub async fn add_guest(
    http: &reqwest::Client,
    api_key: &str,
    event_api_id: &str,
    email: &str,
    name: Option<&str>,
    ticket_type_id: Option<&str>,
) -> Result<AddGuestResponse> {
    let mut guest = serde_json::json!({ "email": email });
    if let Some(n) = name {
        guest["name"] = serde_json::json!(n);
    }

    let mut body = serde_json::json!({
        "event_api_id": event_api_id,
        "guests": [guest],
    });

    // Ticket type goes at root level per Luma v1 docs
    if let Some(tt) = ticket_type_id {
        body["ticket"] = serde_json::json!({ "event_ticket_type_id": tt });
    }

    let resp = http
        .post(format!("{}/event/add-guests", LUMA_BASE))
        .header("x-luma-api-key", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("Luma add-guests request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Luma add-guests failed ({}): {}", status, body));
    }

    // v1 API returns {} on success; extract guest data if present, otherwise treat as success
    let data: AddGuestsResponseWrapper = resp
        .json()
        .await
        .map_err(|e| anyhow!("Failed to parse Luma add-guests response: {}", e))?;

    let guest = data
        .entries
        .and_then(|e| e.into_iter().next())
        .and_then(|e| e.guest);

    Ok(AddGuestResponse {
        api_id: guest.as_ref().and_then(|g| g.api_id.clone()),
        approval_status: guest.as_ref().and_then(|g| g.approval_status.clone()),
    })
}

pub async fn get_guest(
    http: &reqwest::Client,
    api_key: &str,
    event_api_id: &str,
    email: &str,
) -> Result<Option<LumaGuest>> {
    let resp = http
        .get(format!("{}/event/get-guests", LUMA_BASE))
        .header("x-luma-api-key", api_key)
        .query(&[("event_api_id", event_api_id), ("email", email)])
        .send()
        .await
        .map_err(|e| anyhow!("Luma get-guest request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Luma get-guest failed ({}): {}", status, body));
    }

    let data: GetGuestResponseWrapper = resp
        .json()
        .await
        .map_err(|e| anyhow!("Failed to parse Luma get-guest response: {}", e))?;

    let guest = data
        .entries
        .and_then(|e| e.into_iter().next())
        .and_then(|e| e.guest);

    Ok(guest.map(|g| LumaGuest {
        api_id: g.api_id,
        approval_status: g.approval_status,
        check_in_qr_code: g.check_in_qr_code,
        event_ticket: g.event_ticket,
    }))
}
