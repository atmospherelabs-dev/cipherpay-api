use crate::config::Config;

pub async fn send_recovery_email(config: &Config, to: &str, token: &str) -> anyhow::Result<()> {
    let from = config.smtp_from.as_deref()
        .ok_or_else(|| anyhow::anyhow!("SMTP_FROM not configured"))?;
    let api_key = config.smtp_pass.as_deref()
        .ok_or_else(|| anyhow::anyhow!("SMTP_PASS (Resend API key) not configured"))?;

    let frontend_url = config.frontend_url.as_deref().unwrap_or("http://localhost:3000");
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
