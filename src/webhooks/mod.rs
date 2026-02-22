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
) -> anyhow::Result<()> {
    let merchant_row = sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT m.webhook_url, m.webhook_secret FROM invoices i
         JOIN merchants m ON i.merchant_id = m.id
         WHERE i.id = ?"
    )
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?;

    let (webhook_url, webhook_secret) = match merchant_row {
        Some((Some(url), secret)) if !url.is_empty() => (url, secret),
        _ => return Ok(()),
    };

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
        "INSERT INTO webhook_deliveries (id, invoice_id, url, payload, status, attempts, last_attempt_at, next_retry_at)
         VALUES (?, ?, ?, ?, 'pending', 1, ?, ?)"
    )
    .bind(&delivery_id)
    .bind(invoice_id)
    .bind(&webhook_url)
    .bind(&payload_str)
    .bind(&timestamp)
    .bind(&next_retry)
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
            sqlx::query("UPDATE webhook_deliveries SET status = 'delivered' WHERE id = ?")
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::info!(invoice_id, event, "Webhook delivered");
        }
        Ok(resp) => {
            tracing::warn!(invoice_id, event, status = %resp.status(), "Webhook rejected, will retry");
        }
        Err(e) => {
            tracing::warn!(invoice_id, event, error = %e, "Webhook failed, will retry");
        }
    }

    Ok(())
}

pub async fn retry_failed(pool: &SqlitePool, http: &reqwest::Client) -> anyhow::Result<()> {
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

    for (id, url, payload, secret, attempts) in rows {
        let body: serde_json::Value = serde_json::from_str(&payload)?;
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let signature = sign_payload(&secret, &ts, &payload);

        match http.post(&url)
            .header("X-CipherPay-Signature", &signature)
            .header("X-CipherPay-Timestamp", &ts)
            .json(&body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                sqlx::query("UPDATE webhook_deliveries SET status = 'delivered' WHERE id = ?")
                    .bind(&id)
                    .execute(pool)
                    .await?;
                tracing::info!(delivery_id = %id, "Webhook retry delivered");
            }
            _ => {
                let new_attempts = attempts + 1;
                if new_attempts >= 5 {
                    sqlx::query(
                        "UPDATE webhook_deliveries SET status = 'failed', attempts = ?, last_attempt_at = ? WHERE id = ?"
                    )
                    .bind(new_attempts)
                    .bind(&ts)
                    .bind(&id)
                    .execute(pool)
                    .await?;
                    tracing::warn!(delivery_id = %id, "Webhook permanently failed after 5 attempts");
                } else {
                    let next = (Utc::now() + chrono::Duration::seconds(retry_delay_secs(new_attempts)))
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string();
                    sqlx::query(
                        "UPDATE webhook_deliveries SET attempts = ?, last_attempt_at = ?, next_retry_at = ? WHERE id = ?"
                    )
                    .bind(new_attempts)
                    .bind(&ts)
                    .bind(&next)
                    .bind(&id)
                    .execute(pool)
                    .await?;
                    tracing::info!(delivery_id = %id, attempt = new_attempts, next_retry = %next, "Webhook retry scheduled");
                }
            }
        }
    }

    Ok(())
}
