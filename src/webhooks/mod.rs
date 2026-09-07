use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::SqlitePool;
use uuid::Uuid;

static HOST_ATTEMPTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
pub fn wake() {
    WAKE.notify_one();
}
pub async fn notified() {
    WAKE.notified().await;
}

type HmacSha256 = Hmac<Sha256>;

pub fn sign_payload_public(secret: &str, timestamp: &str, payload: &str) -> String {
    sign_payload(secret, timestamp, payload)
}

fn sign_payload(secret: &str, timestamp: &str, payload: &str) -> String {
    let message = format!("{}.{}", timestamp, payload);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn retry_delay_secs(attempt: i64) -> i64 {
    match attempt {
        1 => 60,    // 1 min
        2 => 300,   // 5 min
        3 => 1500,  // 25 min
        4 => 7200,  // 2 hours
        _ => 36000, // 10 hours
    }
}

/// Retry delay with up to +25% jitter, so deliveries that failed around the
/// same time don't all come due on the same retry-worker tick. Bursts of
/// same-host retries look like abuse to per-IP rate limiters (e.g.
/// WordPress.com returns sticky 429s for the whole source IP).
fn jittered_retry_delay_secs(attempt: i64) -> i64 {
    let base = retry_delay_secs(attempt);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(0);
    base + nanos % (base / 4).max(1)
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
        "SELECT webhook_url, webhook_secret FROM merchants WHERE id = ?",
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
        obj.insert(
            "event".to_string(),
            serde_json::Value::String(event.to_string()),
        );
        obj.insert(
            "timestamp".to_string(),
            serde_json::Value::String(timestamp.clone()),
        );
    }

    let payload_str = payload.to_string();
    let signature = sign_payload(&webhook_secret, &timestamp, &payload_str);

    let delivery_id = Uuid::new_v4().to_string();
    let next_retry = (Utc::now() + chrono::Duration::seconds(jittered_retry_delay_secs(1)))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let invoice_id_for_fk = payload.get("invoice_id").and_then(|v| v.as_str());

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

    match http
        .post(&webhook_url)
        .header("X-CipherPay-Signature", &signature)
        .header("X-CipherPay-Timestamp", &timestamp)
        .header("X-CipherPay-Delivery-Id", &delivery_id)
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
            tracing::info!(merchant_id, event, "Lifecycle webhook delivered");
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
            tracing::warn!(merchant_id, event, status = %resp.status(), "Lifecycle webhook rejected, will retry");
        }
        Err(e) => {
            let error_text = e.to_string();
            sqlx::query("UPDATE webhook_deliveries SET response_status = 0, response_error = ? WHERE id = ?")
                .bind(&error_text)
                .bind(&delivery_id)
                .execute(pool)
                .await?;
            tracing::warn!(merchant_id, event, error = %e, "Lifecycle webhook failed, will retry");
        }
    }

    Ok(())
}

pub async fn retry_failed(
    pool: &SqlitePool,
    http: &reqwest::Client,
    encryption_key: &str,
) -> anyhow::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let rows = sqlx::query_as::<_, (String, String, String, String, i64)>(
        "WITH ready AS (
            SELECT wd.id, wd.url, wd.payload, m.webhook_secret, wd.attempts,
                ROW_NUMBER() OVER (PARTITION BY wd.merchant_id ORDER BY wd.next_retry_at, wd.created_at) AS position
            FROM webhook_deliveries wd JOIN merchants m ON wd.merchant_id = m.id
            WHERE wd.status = 'pending' AND wd.attempts < 10
              AND (wd.next_retry_at IS NULL OR wd.next_retry_at <= ?)
        ) SELECT id, url, payload, webhook_secret, attempts FROM ready WHERE position = 1 LIMIT 200",
    )
    .bind(&now)
    .fetch_all(pool)
    .await?;

    // One attempt per destination host per cycle. Back-to-back requests to
    // the same host look like a burst to per-IP rate limiters and can put
    // our IP in a penalty window that then rejects every delivery. Skipped
    // rows stay pending (attempts untouched) and are due again next cycle,
    // giving a natural >=60s spacing between same-host attempts.
    let mut attempted_hosts: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (id, url, payload, raw_secret, attempts) in rows {
        if let Some(host) = url::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
        {
            if !attempted_hosts.insert(host.clone()) {
                continue;
            }
            let interval = if host == "connect.cipherpay.app" {
                5
            } else {
                60
            };
            let mut attempts = HOST_ATTEMPTS.lock().unwrap_or_else(|e| e.into_inner());
            attempts.retain(|_, last| last.elapsed().as_secs() < 60);
            if attempts
                .get(&host)
                .is_some_and(|last| last.elapsed().as_secs() < interval)
            {
                continue;
            }
            attempts.insert(host, std::time::Instant::now());
        }
        let claimed = sqlx::query("UPDATE webhook_deliveries SET next_retry_at = strftime('%Y-%m-%dT%H:%M:%SZ','now','+60 seconds')
            WHERE id = ? AND status = 'pending' AND (next_retry_at IS NULL OR next_retry_at <= ?)")
            .bind(&id).bind(&now).execute(pool).await?;
        if claimed.rows_affected() == 0 {
            continue;
        }
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
            obj.insert(
                "timestamp".to_string(),
                serde_json::Value::String(ts.clone()),
            );
        }
        let updated_payload = body.to_string();
        let signature = sign_payload(&secret, &ts, &updated_payload);

        let (resp_status, resp_error, success, retry_after_secs) = match http
            .post(&url)
            .header("X-CipherPay-Signature", &signature)
            .header("X-CipherPay-Timestamp", &ts)
            .header("X-CipherPay-Delivery-Id", &id)
            .json(&body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                (resp.status().as_u16() as i32, None, true, None)
            }
            Ok(resp) => {
                // Honor Retry-After (seconds form) on rejection so we back
                // off as long as the endpoint asks, never sooner than our
                // own schedule.
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<i64>().ok());
                (
                    resp.status().as_u16() as i32,
                    Some(format!("HTTP {}", resp.status())),
                    false,
                    retry_after,
                )
            }
            Err(e) => (0, Some(e.to_string()), false, None),
        };

        if success {
            sqlx::query("UPDATE webhook_deliveries SET status = 'delivered', attempts = attempts + 1, last_attempt_at = strftime('%Y-%m-%dT%H:%M:%SZ','now'), response_status = ?, response_error = NULL WHERE id = ?")
                .bind(resp_status)
                .bind(&id)
                .execute(pool)
                .await?;
            tracing::info!(delivery_id = %id, "Webhook retry delivered");
        } else {
            let new_attempts = attempts + 1;
            if new_attempts >= 10 {
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
                tracing::warn!(delivery_id = %id, "Webhook permanently failed after 10 attempts");
            } else {
                let base_delay = jittered_retry_delay_secs(new_attempts);
                // Respect Retry-After up to the 10h max backoff, but never
                // retry sooner than our own schedule.
                let delay = retry_after_secs
                    .map(|ra| base_delay.max(ra.min(36000)))
                    .unwrap_or(base_delay);
                let next = (Utc::now() + chrono::Duration::seconds(delay))
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
