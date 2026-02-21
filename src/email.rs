use crate::config::Config;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

pub async fn send_recovery_email(config: &Config, to: &str, token: &str) -> anyhow::Result<()> {
    let smtp_host = config.smtp_host.as_deref()
        .ok_or_else(|| anyhow::anyhow!("SMTP not configured"))?;
    let from = config.smtp_from.as_deref()
        .ok_or_else(|| anyhow::anyhow!("SMTP_FROM not configured"))?;

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

    let email = Message::builder()
        .from(from.parse()?)
        .to(to.parse()?)
        .subject("CipherPay: Account Recovery")
        .header(ContentType::TEXT_PLAIN)
        .body(body)?;

    let mut transport_builder = AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)?;

    if let (Some(user), Some(pass)) = (&config.smtp_user, &config.smtp_pass) {
        transport_builder = transport_builder.credentials(Credentials::new(user.clone(), pass.clone()));
    }

    let mailer = transport_builder.build();
    mailer.send(email).await?;

    tracing::info!(to, "Recovery email sent");
    Ok(())
}
