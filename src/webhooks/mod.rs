use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::SqlitePool;
use uuid::Uuid;
use chrono::Utc;

type HmacSha256 = Hmac<Sha256>;

fn sign_payload(secret: &str, timestamp: &str, payload: &str) -> String {
    let message = format!("{}.{}", timestamp, payload);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn retry_delay_secs(attempt: i64) -> i64 {
    match attempt {
        1 => 60,       // 1 min
        2 => 300,      // 5 min
        3 => 1500,     // 25 min
        4 => 7200,     // 2 hours
        _ => 36000,    // 10 hours
    }
}

pub async fn dispatch(
    pool: &SqlitePool,
    http: &reqwest::Client,
    invoice_id: &str,
    event: &str,
    txid: &str,
    encryption_key: &str,
) -> anyhow::Result<()> {
    let merchant_row = sqlx::query_as::<_, (String, Option<String>, String)>(
        "SELECT m.id, m.webhook_url, m.webhook_secret FROM invoices i
         JOIN merchants m ON i.merchant_id = m.id
         WHERE i.id = ?"
    )
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?;

    let (merchant_id, webhook_url, raw_secret) = match merchant_row {
        Some((mid, Some(url), secret)) if !url.is_empty() => (mid, url, secret),
        _ => return Ok(()),
    };
    let webhook_secret = crate::crypto::decrypt_webhook_secret(&raw_secret, encryption_key)?;

    if let Err(reason) = crate::validation::resolve_and_check_host(&webhook_url) {
        tracing::warn!(invoice_id, url = %webhook_url, %reason, "Webhook blocked: SSRF protection");
        return Ok(());
    }

    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let payload = serde_json::json!({
        "event": event,
        "invoice_id": invoice_id,
        "txid": txid,
        "timestamp": &timestamp,
    });

    let payload_str = payload.to_string();
    let signature = sign_payload(&webhook_secret, &timestamp, &payload_str);

    let delivery_id = Uuid::new_v4().to_string();
    let next_retry = (Utc::now() + chrono::Duration::seconds(retry_delay_secs(1)))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    sqlx::query(
        "INSERT INTO webhook_deliveries (id, invoice_id, url, payload, status, attempts, last_attempt_at, next_retry_at, event_type, merchant_id)
         VALUES (?, ?, ?, ?, 'pending', 1, ?, ?, ?, ?)"
    )
    .bind(&delivery_id)
    .bind(invoice_id)
    .bind(&webhook_url)
    .bind(&payload_str)
    .bind(&timestamp)
    .bind(&next_retry)
    .bind(event)
    .bind(&merchant_id)
    .execute(pool)
    .await?;

    match http.post(&webhook_url)
        .header("X-CipherPay-Signature", &signature)
        .header("X-CipherPay-Timestamp", &timestamp)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let status_code = resp.status().as_u16() as i32;
            sqlx::query("UPDATE webhook_deliveries SET status = 'delivered', response_status = ? WHERE id = ?")
                .bind(status_code)
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::info!(invoice_id, event, "Webhook delivered");
        }
        Ok(resp) => {
            let status_code = resp.status().as_u16() as i32;
            let error_text = format!("HTTP {}", resp.status());
            sqlx::query("UPDATE webhook_deliveries SET response_status = ?, response_error = ? WHERE id = ?")
                .bind(status_code)
                .bind(&error_text)
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::warn!(invoice_id, event, status = %resp.status(), "Webhook rejected, will retry");
        }
        Err(e) => {
            let error_text = e.to_string();
            sqlx::query("UPDATE webhook_deliveries SET response_status = 0, response_error = ? WHERE id = ?")
                .bind(&error_text)
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::warn!(invoice_id, event, error = %e, "Webhook failed, will retry");
        }
    }

    Ok(())
}

pub async fn dispatch_payment(
    pool: &SqlitePool,
    http: &reqwest::Client,
    invoice_id: &str,
    event: &str,
    txid: &str,
    price_zatoshis: i64,
    received_zatoshis: i64,
    overpaid: bool,
    encryption_key: &str,
) -> anyhow::Result<()> {
    let merchant_row = sqlx::query_as::<_, (String, Option<String>, String)>(
        "SELECT m.id, m.webhook_url, m.webhook_secret FROM invoices i
         JOIN merchants m ON i.merchant_id = m.id
         WHERE i.id = ?"
    )
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?;

    let (merchant_id, webhook_url, raw_secret) = match merchant_row {
        Some((mid, Some(url), secret)) if !url.is_empty() => (mid, url, secret),
        _ => return Ok(()),
    };
    let webhook_secret = crate::crypto::decrypt_webhook_secret(&raw_secret, encryption_key)?;

    if let Err(reason) = crate::validation::resolve_and_check_host(&webhook_url) {
        tracing::warn!(invoice_id, url = %webhook_url, %reason, "Webhook blocked: SSRF protection");
        return Ok(());
    }

    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let payload = serde_json::json!({
        "event": event,
        "invoice_id": invoice_id,
        "txid": txid,
        "timestamp": &timestamp,
        "price_zec": crate::invoices::zatoshis_to_zec(price_zatoshis),
        "received_zec": crate::invoices::zatoshis_to_zec(received_zatoshis),
        "overpaid": overpaid,
    });

    let payload_str = payload.to_string();
    let signature = sign_payload(&webhook_secret, &timestamp, &payload_str);

    let delivery_id = Uuid::new_v4().to_string();
    let next_retry = (Utc::now() + chrono::Duration::seconds(retry_delay_secs(1)))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    sqlx::query(
        "INSERT INTO webhook_deliveries (id, invoice_id, url, payload, status, attempts, last_attempt_at, next_retry_at, event_type, merchant_id)
         VALUES (?, ?, ?, ?, 'pending', 1, ?, ?, ?, ?)"
    )
    .bind(&delivery_id)
    .bind(invoice_id)
    .bind(&webhook_url)
    .bind(&payload_str)
    .bind(&timestamp)
    .bind(&next_retry)
    .bind(event)
    .bind(&merchant_id)
    .execute(pool)
    .await?;

    match http.post(&webhook_url)
        .header("X-CipherPay-Signature", &signature)
        .header("X-CipherPay-Timestamp", &timestamp)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let status_code = resp.status().as_u16() as i32;
            sqlx::query("UPDATE webhook_deliveries SET status = 'delivered', response_status = ? WHERE id = ?")
                .bind(status_code)
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::info!(invoice_id, event, "Payment webhook delivered");
        }
        Ok(resp) => {
            let status_code = resp.status().as_u16() as i32;
            let error_text = format!("HTTP {}", resp.status());
            sqlx::query("UPDATE webhook_deliveries SET response_status = ?, response_error = ? WHERE id = ?")
                .bind(status_code)
                .bind(&error_text)
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::warn!(invoice_id, event, status = %resp.status(), "Payment webhook rejected, will retry");
        }
        Err(e) => {
            let error_text = e.to_string();
            sqlx::query("UPDATE webhook_deliveries SET response_status = 0, response_error = ? WHERE id = ?")
                .bind(&error_text)
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::warn!(invoice_id, event, error = %e, "Payment webhook failed, will retry");
        }
    }

    Ok(())
}

/// Dispatch a generic lifecycle event webhook (subscription/invoice events).
/// Unlike dispatch() which is invoice-centric, this takes a merchant_id directly
/// and accepts an arbitrary JSON payload.
pub async fn dispatch_event(
    pool: &SqlitePool,
    http: &reqwest::Client,
    merchant_id: &str,
    event: &str,
    extra: serde_json::Value,
    encryption_key: &str,
) -> anyhow::Result<()> {
    let merchant_row = sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT webhook_url, webhook_secret FROM merchants WHERE id = ?"
    )
    .bind(merchant_id)
    .fetch_optional(pool)
    .await?;

    let (webhook_url, raw_secret) = match merchant_row {
        Some((Some(url), secret)) if !url.is_empty() => (url, secret),
        _ => return Ok(()),
    };
    let webhook_secret = crate::crypto::decrypt_webhook_secret(&raw_secret, encryption_key)?;

    if let Err(reason) = crate::validation::resolve_and_check_host(&webhook_url) {
        tracing::warn!(merchant_id, url = %webhook_url, %reason, "Webhook blocked: SSRF protection");
        return Ok(());
    }

    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut payload = extra;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("event".to_string(), serde_json::Value::String(event.to_string()));
        obj.insert("timestamp".to_string(), serde_json::Value::String(timestamp.clone()));
    }

    let payload_str = payload.to_string();
    let signature = sign_payload(&webhook_secret, &timestamp, &payload_str);

    let delivery_id = Uuid::new_v4().to_string();
    let next_retry = (Utc::now() + chrono::Duration::seconds(retry_delay_secs(1)))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let invoice_id_for_fk = payload.get("invoice_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !invoice_id_for_fk.is_empty() {
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, invoice_id, url, payload, status, attempts, last_attempt_at, next_retry_at, event_type, merchant_id)
             VALUES (?, ?, ?, ?, 'pending', 1, ?, ?, ?, ?)"
        )
        .bind(&delivery_id)
        .bind(invoice_id_for_fk)
        .bind(&webhook_url)
        .bind(&payload_str)
        .bind(&timestamp)
        .bind(&next_retry)
        .bind(event)
        .bind(merchant_id)
        .execute(pool)
        .await?;
    }

    match http.post(&webhook_url)
        .header("X-CipherPay-Signature", &signature)
        .header("X-CipherPay-Timestamp", &timestamp)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let status_code = resp.status().as_u16() as i32;
            if !invoice_id_for_fk.is_empty() {
                sqlx::query("UPDATE webhook_deliveries SET status = 'delivered', response_status = ? WHERE id = ?")
                    .bind(status_code)
                    .bind(&delivery_id)
                    .execute(pool)
                    .await?;
            }
            tracing::info!(merchant_id, event, "Lifecycle webhook delivered");
        }
        Ok(resp) => {
            let status_code = resp.status().as_u16() as i32;
            let error_text = format!("HTTP {}", resp.status());
            if !invoice_id_for_fk.is_empty() {
                sqlx::query("UPDATE webhook_deliveries SET response_status = ?, response_error = ? WHERE id = ?")
                    .bind(status_code)
                    .bind(&error_text)
                    .bind(&delivery_id)
                    .execute(pool)
                    .await?;
            }
            tracing::warn!(merchant_id, event, status = %resp.status(), "Lifecycle webhook rejected, will retry");
        }
        Err(e) => {
            let error_text = e.to_string();
            if !invoice_id_for_fk.is_empty() {
                sqlx::query("UPDATE webhook_deliveries SET response_status = 0, response_error = ? WHERE id = ?")
                    .bind(&error_text)
                    .bind(&delivery_id)
                    .execute(pool)
                    .await?;
            }
            tracing::warn!(merchant_id, event, error = %e, "Lifecycle webhook failed, will retry");
        }
    }

    Ok(())
}

pub async fn retry_failed(pool: &SqlitePool, http: &reqwest::Client, encryption_key: &str) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let rows = sqlx::query_as::<_, (String, String, String, String, i64)>(
        "SELECT wd.id, wd.url, wd.payload, m.webhook_secret, wd.attempts
         FROM webhook_deliveries wd
         JOIN invoices i ON wd.invoice_id = i.id
         JOIN merchants m ON i.merchant_id = m.id
         WHERE wd.status = 'pending'
         AND wd.attempts < 5
         AND (wd.next_retry_at IS NULL OR wd.next_retry_at <= ?)"
    )
    .bind(&now)
    .fetch_all(pool)
    .await?;

    for (id, url, payload, raw_secret, attempts) in rows {
        let secret = match crate::crypto::decrypt_webhook_secret(&raw_secret, encryption_key) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(delivery_id = %id, error = %e, "Failed to decrypt webhook secret, marking delivery failed");
                sqlx::query("UPDATE webhook_deliveries SET status = 'failed', response_error = 'Webhook secret decryption failed' WHERE id = ?")
                    .bind(&id)
                    .execute(pool)
                    .await?;
                continue;
            }
        };
        if let Err(reason) = crate::validation::resolve_and_check_host(&url) {
            tracing::warn!(delivery_id = %id, %url, %reason, "Webhook retry blocked: SSRF protection");
            sqlx::query("UPDATE webhook_deliveries SET status = 'failed' WHERE id = ?")
                .bind(&id)
                .execute(pool)
                .await?;
            continue;
        }

        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut body: serde_json::Value = serde_json::from_str(&payload)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("timestamp".to_string(), serde_json::Value::String(ts.clone()));
        }
        let updated_payload = body.to_string();
        let signature = sign_payload(&secret, &ts, &updated_payload);

        let (resp_status, resp_error, success) = match http.post(&url)
            .header("X-CipherPay-Signature", &signature)
            .header("X-CipherPay-Timestamp", &ts)
            .json(&body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                (resp.status().as_u16() as i32, None, true)
            }
            Ok(resp) => {
                (resp.status().as_u16() as i32, Some(format!("HTTP {}", resp.status())), false)
            }
            Err(e) => {
                (0, Some(e.to_string()), false)
            }
        };

        if success {
            sqlx::query("UPDATE webhook_deliveries SET status = 'delivered', response_status = ?, response_error = NULL WHERE id = ?")
                .bind(resp_status)
                .bind(&id)
                .execute(pool)
                .await?;
            tracing::info!(delivery_id = %id, "Webhook retry delivered");
        } else {
            let new_attempts = attempts + 1;
            if new_attempts >= 5 {
                sqlx::query(
                    "UPDATE webhook_deliveries SET status = 'failed', attempts = ?, last_attempt_at = ?, response_status = ?, response_error = ? WHERE id = ?"
                )
                .bind(new_attempts)
                .bind(&ts)
                .bind(resp_status)
                .bind(&resp_error)
                .bind(&id)
                .execute(pool)
                .await?;
                tracing::warn!(delivery_id = %id, "Webhook permanently failed after 5 attempts");
            } else {
                let next = (Utc::now() + chrono::Duration::seconds(retry_delay_secs(new_attempts)))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string();
                sqlx::query(
                    "UPDATE webhook_deliveries SET attempts = ?, last_attempt_at = ?, next_retry_at = ?, response_status = ?, response_error = ? WHERE id = ?"
                )
                .bind(new_attempts)
                .bind(&ts)
                .bind(&next)
                .bind(resp_status)
                .bind(&resp_error)
                .bind(&id)
                .execute(pool)
                .await?;
                tracing::info!(delivery_id = %id, attempt = new_attempts, next_retry = %next, "Webhook retry scheduled");
            }
        }
    }

    Ok(())
}
