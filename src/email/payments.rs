use anyhow::Context;
use serde_json::{json, Value};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

use crate::{config::Config, crypto, invoices};

const TEMPLATE: &str = "merchant_payment_confirmed";
const MAX_ATTEMPTS: i64 = 8;

/// Queue in the same database statement as confirmation, including confirmations
/// from cross-chain payments. Enabling the setting never emails old invoices.
pub async fn migrate_payment_email_queue(pool: &SqlitePool) -> anyhow::Result<()> {
    crate::db::run_tracked_migration(pool, "payment_email_queue_v2026_10_14", || async {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS payment_email_queue (
                invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
                merchant_id TEXT NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                state TEXT NOT NULL DEFAULT 'pending'
                    CHECK(state IN ('pending','sending','sent','cancelled','failed')),
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                first_attempt_at INTEGER,
                claim_id TEXT,
                payload TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
            )",
        ).execute(&mut *tx).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_payment_email_due ON payment_email_queue(state,next_attempt)")
            .execute(&mut *tx).await?;
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS queue_confirmed_payment_email
             AFTER UPDATE OF status ON invoices
             WHEN NEW.status='confirmed' AND OLD.status!='confirmed'
                  AND NEW.memo_code NOT LIKE 'SETTLE-%'
             BEGIN
                INSERT OR IGNORE INTO payment_email_queue(invoice_id,merchant_id)
                SELECT NEW.id,NEW.merchant_id FROM merchants m
                WHERE m.id=NEW.merchant_id AND m.payment_email_notifications=1
                      AND m.recovery_email IS NOT NULL AND m.recovery_email!=''
                      AND NOT EXISTS(SELECT 1 FROM email_events e
                          WHERE e.merchant_id=NEW.merchant_id
                          AND e.template='merchant_payment_confirmed' AND e.entity_id=NEW.id);
             END",
        ).execute(&mut *tx).await?;
        // Stop queued mail immediately on opt-out or a recipient change. Clear
        // the snapshot so account settings never leave unnecessary PII behind.
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS cancel_payment_emails_on_settings_change
             AFTER UPDATE OF payment_email_notifications,recovery_email,recovery_email_hash ON merchants
             WHEN NEW.payment_email_notifications=0
                  OR NEW.recovery_email_hash IS NOT OLD.recovery_email_hash
                  OR ((NEW.recovery_email_hash IS NULL OR OLD.recovery_email_hash IS NULL)
                      AND NEW.recovery_email IS NOT OLD.recovery_email)
             BEGIN
                UPDATE payment_email_queue SET state='cancelled',payload=NULL,claim_id=NULL
                WHERE merchant_id=NEW.id AND state IN ('pending','sending');
             END",
        ).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }).await
}

#[derive(FromRow)]
struct PendingEmail {
    invoice_id: String,
    merchant_id: String,
    attempts: i64,
    payload: Option<String>,
}

pub async fn run_payment_email_worker(pool: SqlitePool, config: Config) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error, "Could not start payment email worker");
            return;
        }
    };
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        interval.tick().await;
        if !config.smtp_configured() {
            continue;
        }
        // A small batch keeps provider requests bounded. A lease and a stable
        // provider key protect against concurrent workers and process restarts.
        for _ in 0..10 {
            match process_next(&pool, &config, &client, "https://api.resend.com/emails").await {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    tracing::error!(%error, "Payment email queue processing failed");
                    break;
                }
            }
        }
    }
}

async fn process_next(
    pool: &SqlitePool,
    config: &Config,
    client: &reqwest::Client,
    endpoint: &str,
) -> anyhow::Result<bool> {
    if !config.smtp_configured() {
        return Ok(false);
    }
    // Resend retains idempotency keys for 24 hours. Never retry an ambiguous
    // request past that window, including after a long process outage.
    sqlx::query(
        "UPDATE payment_email_queue SET state='failed',payload=NULL,claim_id=NULL
         WHERE state IN ('pending','sending')
               AND ((first_attempt_at IS NOT NULL AND first_attempt_at < strftime('%s','now')-82800)
                    OR (attempts>=? AND next_attempt<=strftime('%s','now')))",
    )
    .bind(MAX_ATTEMPTS)
    .execute(pool)
    .await?;
    let claim_id = Uuid::new_v4().to_string();
    let queued: Option<PendingEmail> = sqlx::query_as(
        "UPDATE payment_email_queue SET state='sending',claim_id=?,
             next_attempt=strftime('%s','now')+120,attempts=attempts+1,
             first_attempt_at=COALESCE(first_attempt_at,strftime('%s','now'))
         WHERE invoice_id=(SELECT invoice_id FROM payment_email_queue
             WHERE state IN ('pending','sending') AND next_attempt<=strftime('%s','now')
             ORDER BY next_attempt,invoice_id LIMIT 1)
         RETURNING invoice_id,merchant_id,attempts,payload",
    )
    .bind(&claim_id)
    .fetch_optional(pool)
    .await?;
    let Some(queued) = queued else {
        return Ok(false);
    };
    match deliver(pool, config, client, endpoint, &queued, &claim_id).await {
        Ok(()) => {}
        Err(error) => {
            // Avoid logging provider response bodies or recipient addresses.
            tracing::warn!(invoice_id=%queued.invoice_id, attempt=queued.attempts, %error,
                "Payment email delivery failed; scheduling retry");
            let delay = (60_i64 * 2_i64.pow((queued.attempts - 1).min(6) as u32)).min(3600);
            sqlx::query(
                "UPDATE payment_email_queue SET state=?,claim_id=NULL,
                     next_attempt=strftime('%s','now')+?,
                     payload=CASE WHEN ? THEN NULL ELSE payload END
                 WHERE invoice_id=? AND claim_id=?",
            )
            .bind(if queued.attempts >= MAX_ATTEMPTS {
                "failed"
            } else {
                "pending"
            })
            .bind(delay)
            .bind(queued.attempts >= MAX_ATTEMPTS)
            .bind(&queued.invoice_id)
            .bind(&claim_id)
            .execute(pool)
            .await?;
        }
    }
    Ok(true)
}

async fn deliver(
    pool: &SqlitePool,
    config: &Config,
    client: &reqwest::Client,
    endpoint: &str,
    queued: &PendingEmail,
    claim_id: &str,
) -> anyhow::Result<()> {
    let enabled: bool =
        sqlx::query_scalar("SELECT payment_email_notifications=1 FROM merchants WHERE id=?")
            .bind(&queued.merchant_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(false);
    let invoice = invoices::get_invoice(pool, &queued.invoice_id).await?;
    if !enabled || invoice.as_ref().is_none_or(|i| i.status != "confirmed") {
        return finish(pool, queued, claim_id, "cancelled").await;
    }
    let already_sent: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM email_events WHERE merchant_id=? AND template=? AND entity_id=?)",
    ).bind(&queued.merchant_id).bind(TEMPLATE).bind(&queued.invoice_id).fetch_one(pool).await?;
    if already_sent {
        return finish(pool, queued, claim_id, "sent").await;
    }
    let email = super::get_merchant_email(pool, &queued.merchant_id, &config.encryption_key)
        .await
        .context("Payment notification recipient unavailable")?;
    let payload: Value = match &queued.payload {
        Some(stored) => {
            let plaintext = if config.encryption_key.is_empty() {
                stored.clone()
            } else {
                crypto::decrypt(stored, &config.encryption_key)?
            };
            serde_json::from_str(&plaintext)?
        }
        None => {
            let invoice = invoice.unwrap();
            let payload = payment_payload(config, &email, &invoice);
            let plaintext = serde_json::to_string(&payload)?;
            let stored = if config.encryption_key.is_empty() {
                plaintext
            } else {
                crypto::encrypt(&plaintext, &config.encryption_key)?
            };
            let saved = sqlx::query(
                "UPDATE payment_email_queue SET payload=? WHERE invoice_id=? AND claim_id=?",
            )
            .bind(stored)
            .bind(&queued.invoice_id)
            .bind(claim_id)
            .execute(pool)
            .await?;
            if saved.rows_affected() == 0 {
                return Ok(());
            }
            payload
        }
    };
    if payload["to"][0].as_str() != Some(email.as_str()) {
        return finish(pool, queued, claim_id, "cancelled").await;
    }
    let still_claimed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM payment_email_queue WHERE invoice_id=? AND claim_id=? AND state='sending')",
    ).bind(&queued.invoice_id).bind(claim_id).fetch_one(pool).await?;
    if !still_claimed {
        return Ok(());
    }
    let response = client
        .post(endpoint)
        .bearer_auth(config.smtp_pass.as_deref().unwrap())
        .header(
            "Idempotency-Key",
            format!("{TEMPLATE}/{}", queued.invoice_id),
        )
        .json(&payload)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "Email provider returned {}",
        response.status()
    );

    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT OR IGNORE INTO email_events(id,merchant_id,template,entity_id,sent_at)
         SELECT ?,q.merchant_id,?,q.invoice_id,strftime('%Y-%m-%dT%H:%M:%SZ','now')
         FROM payment_email_queue q JOIN merchants m ON m.id=q.merchant_id
         WHERE q.invoice_id=? AND q.claim_id=? AND q.state='sending'",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(TEMPLATE)
    .bind(&queued.invoice_id)
    .bind(claim_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE payment_email_queue SET state='sent',payload=NULL,claim_id=NULL WHERE invoice_id=? AND claim_id=?")
        .bind(&queued.invoice_id).bind(claim_id).execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::info!(invoice_id=%queued.invoice_id, "Merchant payment notification sent");
    Ok(())
}

async fn finish(
    pool: &SqlitePool,
    queued: &PendingEmail,
    claim_id: &str,
    state: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE payment_email_queue SET state=?,payload=NULL,claim_id=NULL WHERE invoice_id=? AND claim_id=?")
        .bind(state).bind(&queued.invoice_id).bind(claim_id).execute(pool).await?;
    Ok(())
}

fn payment_payload(config: &Config, email: &str, invoice: &invoices::Invoice) -> Value {
    let url = config
        .frontend_url
        .as_deref()
        .unwrap_or("https://cipherpay.app")
        .trim_end_matches('/');
    let body = format!(
        "CipherPay: Customer Payment Confirmed\n\n\
         A customer payment has been confirmed.\n\n\
         Reference: {}\n\
         Invoice ID: {}\n\
         Invoice amount: {}.{:08} ZEC\n\n\
         View the payment in your dashboard:\n{}/dashboard\n\n\
         Manage payment email notifications in dashboard Settings.\n\n— CipherPay",
        invoice.memo_code,
        invoice.id,
        invoice.price_zatoshis / 100_000_000,
        invoice.price_zatoshis % 100_000_000,
        url,
    );
    json!({"from": config.smtp_from.as_deref().unwrap(), "to": [email],
        "subject": "CipherPay: Customer Payment Confirmed", "text": body})
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn setup() -> (SqlitePool, Config) {
        let pool = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&pool, "merchant").await;
        let mut config = Config::from_env().unwrap();
        config.encryption_key = "11".repeat(32);
        config.smtp_pass = Some("test-api-key".into());
        config.smtp_from = Some("CipherPay <payments@example.com>".into());
        config.frontend_url = Some("https://example.com".into());
        (pool, config)
    }

    async fn opt_in(pool: &SqlitePool, config: &Config) {
        let email = crypto::encrypt("merchant@example.com", &config.encryption_key).unwrap();
        sqlx::query("UPDATE merchants SET recovery_email=?,payment_email_notifications=1 WHERE id='merchant'")
            .bind(email).execute(pool).await.unwrap();
    }

    async fn confirm(pool: &SqlitePool, id: &str) {
        invoices::mark_detected(pool, id, "private-txid", 1000)
            .await
            .unwrap();
        assert!(invoices::mark_confirmed(pool, id, None, None)
            .await
            .unwrap());
    }

    async fn count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM payment_email_queue")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn server(statuses: Vec<u16>) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/emails", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut stream, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut request = Vec::new();
                let (header, body) = loop {
                    let mut chunk = [0_u8; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8(request[..end].to_vec()).unwrap();
                        let length: usize = header
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break (
                                header,
                                serde_json::from_slice(&request[end + 4..end + 4 + length])
                                    .unwrap(),
                            );
                        }
                    }
                };
                requests.push((header, body));
                stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
            }
            requests
        });
        (endpoint, handle)
    }

    #[tokio::test]
    async fn queues_only_new_opted_in_customer_confirmations_once() {
        let (pool, config) = setup().await;
        crate::repair_tests::invoice(&pool, "old", "merchant").await;
        confirm(&pool, "old").await;
        assert_eq!(count(&pool).await, 0);
        opt_in(&pool, &config).await;
        assert_eq!(count(&pool).await, 0); // No historical backfill.
        crate::repair_tests::invoice(&pool, "partial", "merchant").await;
        invoices::mark_underpaid(&pool, "partial", 400, "first")
            .await
            .unwrap();
        assert_eq!(count(&pool).await, 0);
        invoices::mark_detected(&pool, "partial", "second", 1000)
            .await
            .unwrap();
        assert_eq!(count(&pool).await, 0);
        assert!(invoices::mark_confirmed(&pool, "partial", None, None)
            .await
            .unwrap());
        assert!(!invoices::mark_confirmed(&pool, "partial", None, None)
            .await
            .unwrap());
        sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='partial'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(count(&pool).await, 1);
        crate::repair_tests::invoice(&pool, "billing", "merchant").await;
        sqlx::query("UPDATE invoices SET memo_code='SETTLE-cycle' WHERE id='billing'")
            .execute(&pool)
            .await
            .unwrap();
        confirm(&pool, "billing").await;
        assert_eq!(count(&pool).await, 1);
        sqlx::query("UPDATE merchants SET recovery_email=NULL WHERE id='merchant'")
            .execute(&pool)
            .await
            .unwrap();
        crate::repair_tests::invoice(&pool, "no-email", "merchant").await;
        confirm(&pool, "no-email").await;
        assert_eq!(count(&pool).await, 1);
    }

    #[tokio::test]
    async fn retries_with_stable_encrypted_payload_and_records_delivery_once() {
        let (pool, mut config) = setup().await;
        opt_in(&pool, &config).await;
        crate::repair_tests::invoice(&pool, "payment", "merchant").await;
        confirm(&pool, "payment").await;
        let (endpoint, handle) = server(vec![503, 200]).await;
        let client = reqwest::Client::new();
        assert!(process_next(&pool, &config, &client, &endpoint)
            .await
            .unwrap());
        let (state, stored, attempts, due): (String, String, i64, bool) = sqlx::query_as(
            "SELECT state,payload,attempts,next_attempt>strftime('%s','now') FROM payment_email_queue",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(state, "pending");
        assert_eq!(attempts, 1);
        assert!(due);
        assert!(!stored.contains("merchant@example.com"));
        assert!(crypto::decrypt(&stored, &config.encryption_key)
            .unwrap()
            .contains("merchant@example.com"));
        assert!(!process_next(&pool, &config, &client, &endpoint)
            .await
            .unwrap());
        sqlx::query("UPDATE payment_email_queue SET next_attempt=0")
            .execute(&pool)
            .await
            .unwrap();
        config.smtp_from = Some("changed@example.com".into());
        config.frontend_url = Some("https://changed.example.com".into());
        assert!(process_next(&pool, &config, &client, &endpoint)
            .await
            .unwrap());
        assert!(!process_next(&pool, &config, &client, &endpoint)
            .await
            .unwrap());
        let requests = handle.await.unwrap();
        assert_eq!(requests[0].1, requests[1].1);
        for (header, body) in &requests {
            assert!(header
                .to_lowercase()
                .contains("idempotency-key: merchant_payment_confirmed/payment"));
            let body = body["text"].as_str().unwrap();
            assert!(body.contains("Invoice amount: 0.00001000 ZEC"));
            assert!(!body.contains("private-txid"));
            assert!(!body.contains("test-address"));
        }
        let (state, payload): (String, Option<String>) =
            sqlx::query_as("SELECT state,payload FROM payment_email_queue")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "sent");
        assert!(payload.is_none());
        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM email_events WHERE template='merchant_payment_confirmed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 1);
    }

    #[tokio::test]
    async fn opt_out_recipient_change_refund_and_deletion_suppress_queued_mail() {
        let (pool, config) = setup().await;
        opt_in(&pool, &config).await;
        crate::repair_tests::invoice(&pool, "opt-out", "merchant").await;
        confirm(&pool, "opt-out").await;
        sqlx::query("UPDATE merchants SET payment_email_notifications=0 WHERE id='merchant'")
            .execute(&pool)
            .await
            .unwrap();
        opt_in(&pool, &config).await;
        crate::repair_tests::invoice(&pool, "new-recipient", "merchant").await;
        confirm(&pool, "new-recipient").await;
        sqlx::query("UPDATE merchants SET recovery_email='new@example.com' WHERE id='merchant'")
            .execute(&pool)
            .await
            .unwrap();
        let cancelled: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM payment_email_queue WHERE state='cancelled'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cancelled, 2);
        crate::repair_tests::invoice(&pool, "refunded", "merchant").await;
        confirm(&pool, "refunded").await;
        invoices::mark_refunded(&pool, "refunded", None)
            .await
            .unwrap();
        let client = reqwest::Client::new();
        assert!(process_next(&pool, &config, &client, "http://127.0.0.1:1")
            .await
            .unwrap());
        assert!(!process_next(&pool, &config, &client, "http://127.0.0.1:1")
            .await
            .unwrap());
        crate::repair_tests::invoice(&pool, "deleted", "merchant").await;
        confirm(&pool, "deleted").await;
        assert_eq!(count(&pool).await, 4);
        crate::merchants::delete_merchant(&pool, "merchant")
            .await
            .unwrap();
        assert_eq!(count(&pool).await, 0);
    }

    #[tokio::test]
    async fn concurrent_workers_claim_once_and_old_ambiguous_attempts_expire() {
        let (pool, config) = setup().await;
        opt_in(&pool, &config).await;
        crate::repair_tests::invoice(&pool, "concurrent", "merchant").await;
        confirm(&pool, "concurrent").await;
        let (endpoint, handle) = server(vec![200]).await;
        let client = reqwest::Client::new();
        let (a, b) = tokio::join!(
            process_next(&pool, &config, &client, &endpoint),
            process_next(&pool, &config, &client, &endpoint)
        );
        assert_eq!(a.unwrap() as u8 + b.unwrap() as u8, 1);
        assert_eq!(handle.await.unwrap().len(), 1);
        crate::repair_tests::invoice(&pool, "too-old", "merchant").await;
        confirm(&pool, "too-old").await;
        sqlx::query("UPDATE payment_email_queue SET state='sending',first_attempt_at=strftime('%s','now')-86400,next_attempt=0 WHERE invoice_id='too-old'")
            .execute(&pool).await.unwrap();
        assert!(!process_next(&pool, &config, &client, "http://127.0.0.1:1")
            .await
            .unwrap());
        let state: String =
            sqlx::query_scalar("SELECT state FROM payment_email_queue WHERE invoice_id='too-old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "failed");
    }

    #[tokio::test]
    async fn unconfigured_email_keeps_queue_and_crashed_workers_have_bounded_retries() {
        let (pool, mut config) = setup().await;
        opt_in(&pool, &config).await;
        crate::repair_tests::invoice(&pool, "waiting", "merchant").await;
        confirm(&pool, "waiting").await;
        let client = reqwest::Client::new();
        config.smtp_pass = None;
        assert!(!process_next(&pool, &config, &client, "http://127.0.0.1:1")
            .await
            .unwrap());
        let attempts: i64 = sqlx::query_scalar("SELECT attempts FROM payment_email_queue")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 0);
        config.smtp_pass = Some("test-api-key".into());
        sqlx::query("UPDATE payment_email_queue SET state='sending',attempts=8,next_attempt=0")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!process_next(&pool, &config, &client, "http://127.0.0.1:1")
            .await
            .unwrap());
        let state: String = sqlx::query_scalar("SELECT state FROM payment_email_queue")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[tokio::test]
    async fn queued_notification_survives_database_reopen_and_expired_worker_lease() {
        let path = std::env::temp_dir().join(format!("cipherpay-email-{}.db", Uuid::new_v4()));
        let url = format!("sqlite:{}", path.display());
        let pool = crate::db::create_pool(&url).await.unwrap();
        crate::repair_tests::merchant(&pool, "merchant").await;
        let (_, config) = setup().await;
        opt_in(&pool, &config).await;
        crate::repair_tests::invoice(&pool, "restart", "merchant").await;
        confirm(&pool, "restart").await;
        sqlx::query("UPDATE payment_email_queue SET state='sending',attempts=1,claim_id='crashed',next_attempt=0,first_attempt_at=strftime('%s','now')")
            .execute(&pool).await.unwrap();
        pool.close().await;
        let pool = crate::db::create_pool(&url).await.unwrap();
        assert_eq!(count(&pool).await, 1);
        let (endpoint, handle) = server(vec![200]).await;
        assert!(
            process_next(&pool, &config, &reqwest::Client::new(), &endpoint)
                .await
                .unwrap()
        );
        assert_eq!(handle.await.unwrap().len(), 1);
        let state: String = sqlx::query_scalar("SELECT state FROM payment_email_queue")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "sent");
        pool.close().await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn saving_the_same_address_keeps_queued_notifications() {
        let (pool, config) = setup().await;
        opt_in(&pool, &config).await;
        let hash = crypto::blind_index("merchant@example.com", &config.encryption_key);
        sqlx::query("UPDATE merchants SET recovery_email_hash=? WHERE id='merchant'")
            .bind(&hash)
            .execute(&pool)
            .await
            .unwrap();
        crate::repair_tests::invoice(&pool, "same-address", "merchant").await;
        confirm(&pool, "same-address").await;
        sqlx::query(
            "UPDATE merchants SET recovery_email=?,recovery_email_hash=? WHERE id='merchant'",
        )
        .bind(crypto::encrypt("merchant@example.com", &config.encryption_key).unwrap())
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM payment_email_queue")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "pending");
    }
}
