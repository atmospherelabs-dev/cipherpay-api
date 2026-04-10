use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;

pub async fn send_recovery_email(config: &Config, to: &str, token: &str) -> anyhow::Result<()> {
    let from = config
        .smtp_from
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("SMTP_FROM not configured"))?;
    let api_key = config
        .smtp_pass
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("SMTP_PASS (Resend API key) not configured"))?;

    let frontend_url = config
        .frontend_url
        .as_deref()
        .unwrap_or("http://localhost:3000");
    let recovery_link = format!("{}/dashboard/recover/confirm?token={}", frontend_url, token);

    let body = format!(
        "CipherPay Account Recovery\n\
         \n\
         Someone requested a recovery link for the merchant account associated with this email.\n\
         \n\
         Click the link below to get a new dashboard token:\n\
         {}\n\
         \n\
         This link expires in 1 hour.\n\
         \n\
         If you did not request this, you can safely ignore this email.\n\
         \n\
         — CipherPay",
        recovery_link
    );

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "from": from,
            "to": [to],
            "subject": "CipherPay: Account Recovery",
            "text": body,
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        tracing::error!(status = %status, body = %err_body, "Resend API error");
        anyhow::bail!("Email send failed ({})", status);
    }

    tracing::info!("Recovery email sent");
    Ok(())
}

// --- Billing email infrastructure ---

/// Check if an email for this (merchant, template, entity) was already sent.
/// Returns true if it already exists (skip sending).
async fn email_already_sent(
    pool: &SqlitePool,
    merchant_id: &str,
    template: &str,
    entity_id: &str,
) -> bool {
    sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM email_events WHERE merchant_id = ? AND template = ? AND entity_id = ?",
    )
    .bind(merchant_id)
    .bind(template)
    .bind(entity_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
        > 0
}

/// Record that an email was sent (insert-or-ignore for idempotency).
async fn record_email_sent(pool: &SqlitePool, merchant_id: &str, template: &str, entity_id: &str) {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "INSERT OR IGNORE INTO email_events (id, merchant_id, template, entity_id, sent_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(template)
    .bind(entity_id)
    .bind(&now)
    .execute(pool)
    .await
    .ok();
}

/// Send a billing email via Resend, with idempotency check.
/// Returns Ok(true) if sent, Ok(false) if already sent or no email configured.
async fn send_billing_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    template: &str,
    entity_id: &str,
    subject: &str,
    body: &str,
) -> anyhow::Result<bool> {
    if !config.smtp_configured() {
        return Ok(false);
    }

    if email_already_sent(pool, merchant_id, template, entity_id).await {
        return Ok(false);
    }

    let from = config.smtp_from.as_deref().unwrap();
    let api_key = config.smtp_pass.as_deref().unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "from": from,
            "to": [to],
            "subject": subject,
            "text": body,
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        tracing::error!(
            status = %status,
            body = %err_body,
            template,
            merchant_id,
            "Billing email send failed"
        );
        return Ok(false);
    }

    record_email_sent(pool, merchant_id, template, entity_id).await;
    tracing::info!(template, merchant_id, "Billing email sent");
    Ok(true)
}

pub async fn send_settlement_invoice_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    cycle_id: &str,
    amount_zec: f64,
    due_date: &str,
    grace_days: i64,
) -> anyhow::Result<bool> {
    let dashboard_url = config
        .frontend_url
        .as_deref()
        .unwrap_or("https://cipherpay.app");

    let body = format!(
        "CipherPay Billing\n\
         \n\
         Your billing cycle has closed.\n\
         \n\
         Outstanding balance: {:.8} ZEC\n\
         Due by: {} ({} days)\n\
         \n\
         Please settle before the due date to avoid service interruption.\n\
         \n\
         View your billing details:\n\
         {}/dashboard\n\
         \n\
         — CipherPay",
        amount_zec, due_date, grace_days, dashboard_url
    );

    send_billing_email(
        pool,
        config,
        to,
        merchant_id,
        "settlement_invoice",
        cycle_id,
        "CipherPay: Billing Cycle Closed",
        &body,
    )
    .await
}

pub async fn send_billing_reminder_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    cycle_id: &str,
    amount_zec: f64,
    due_date: &str,
    days_remaining: i64,
) -> anyhow::Result<bool> {
    let dashboard_url = config
        .frontend_url
        .as_deref()
        .unwrap_or("https://cipherpay.app");

    let body = format!(
        "CipherPay Billing Reminder\n\
         \n\
         Your outstanding balance of {:.8} ZEC is due in {} days ({}).\n\
         \n\
         After that date, new invoice creation will be paused until payment is received.\n\
         \n\
         View your billing details:\n\
         {}/dashboard\n\
         \n\
         — CipherPay",
        amount_zec, days_remaining, due_date, dashboard_url
    );

    send_billing_email(
        pool,
        config,
        to,
        merchant_id,
        "grace_reminder",
        cycle_id,
        "CipherPay: Billing Payment Reminder",
        &body,
    )
    .await
}

pub async fn send_past_due_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    cycle_id: &str,
    amount_zec: f64,
) -> anyhow::Result<bool> {
    let dashboard_url = config
        .frontend_url
        .as_deref()
        .unwrap_or("https://cipherpay.app");

    let body = format!(
        "CipherPay: Account Past Due\n\
         \n\
         Your billing payment is overdue. Invoice creation has been paused.\n\
         \n\
         Outstanding: {:.8} ZEC\n\
         \n\
         Pay now to restore full access:\n\
         {}/dashboard\n\
         \n\
         — CipherPay",
        amount_zec, dashboard_url
    );

    send_billing_email(
        pool,
        config,
        to,
        merchant_id,
        "past_due",
        cycle_id,
        "CipherPay: Account Past Due",
        &body,
    )
    .await
}

pub async fn send_suspended_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    cycle_id: &str,
    amount_zec: f64,
) -> anyhow::Result<bool> {
    let dashboard_url = config
        .frontend_url
        .as_deref()
        .unwrap_or("https://cipherpay.app");

    let body = format!(
        "CipherPay: Account Suspended\n\
         \n\
         Your account has been suspended due to an unpaid balance of {:.8} ZEC.\n\
         All services (invoices, webhooks, sessions, x402) are paused.\n\
         \n\
         Pay now to restore your account:\n\
         {}/dashboard\n\
         \n\
         — CipherPay",
        amount_zec, dashboard_url
    );

    send_billing_email(
        pool,
        config,
        to,
        merchant_id,
        "suspended",
        cycle_id,
        "CipherPay: Account Suspended",
        &body,
    )
    .await
}

pub async fn send_payment_confirmed_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    cycle_id: &str,
) -> anyhow::Result<bool> {
    let body = "CipherPay: Payment Received\n\
         \n\
         Your settlement payment has been confirmed. Your account is fully active.\n\
         \n\
         Thank you.\n\
         \n\
         — CipherPay"
        .to_string();

    send_billing_email(
        pool,
        config,
        to,
        merchant_id,
        "payment_confirmed",
        cycle_id,
        "CipherPay: Payment Received",
        &body,
    )
    .await
}

pub async fn send_discount_expiry_warning_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    new_rate_pct: f64,
    effective_date: &str,
) -> anyhow::Result<bool> {
    let body = format!(
        "CipherPay: Fee Rate Change\n\
         \n\
         Your reduced fee rate ends on {}.\n\
         Starting that date, your fee rate will return to the standard {:.1}%.\n\
         \n\
         No action needed — this is just a heads-up.\n\
         \n\
         — CipherPay",
        effective_date,
        new_rate_pct * 100.0
    );

    send_billing_email(
        pool,
        config,
        to,
        merchant_id,
        "discount_expiry_warning",
        merchant_id,
        "CipherPay: Fee Rate Change Coming",
        &body,
    )
    .await
}

pub async fn send_discount_expired_email(
    pool: &SqlitePool,
    config: &Config,
    to: &str,
    merchant_id: &str,
    new_rate_pct: f64,
) -> anyhow::Result<bool> {
    let body = format!(
        "CipherPay: Fee Rate Updated\n\
         \n\
         Your fee rate is now {:.1}% (standard rate).\n\
         Your reduced rate period has ended. Thank you for being a CipherPay merchant.\n\
         \n\
         — CipherPay",
        new_rate_pct * 100.0
    );

    send_billing_email(
        pool,
        config,
        to,
        merchant_id,
        "discount_expired",
        merchant_id,
        "CipherPay: Fee Rate Updated",
        &body,
    )
    .await
}

/// Look up a merchant's decrypted recovery email for billing notifications.
pub async fn get_merchant_email(
    pool: &SqlitePool,
    merchant_id: &str,
    encryption_key: &str,
) -> Option<String> {
    let encrypted: Option<String> =
        sqlx::query_scalar("SELECT recovery_email FROM merchants WHERE id = ?")
            .bind(merchant_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();

    encrypted.and_then(|e| {
        crate::crypto::decrypt_email(&e, encryption_key)
            .ok()
            .filter(|email| !email.is_empty())
    })
}
