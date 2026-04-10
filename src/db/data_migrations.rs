use sqlx::SqlitePool;

/// Encrypt any plaintext recovery emails and backfill blind-index hashes.
/// Called once at startup when ENCRYPTION_KEY is set.
/// Plaintext emails are identified by containing '@'.
pub async fn migrate_encrypt_recovery_emails(
    pool: &SqlitePool,
    encryption_key: &str,
) -> anyhow::Result<()> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, recovery_email FROM merchants WHERE recovery_email IS NOT NULL AND recovery_email != '' AND (recovery_email_hash IS NULL OR recovery_email_hash = '')"
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(
        count = rows.len(),
        "Migrating recovery emails (encrypt + blind index)"
    );
    for (id, email_raw) in &rows {
        let plaintext = if email_raw.contains('@') {
            email_raw.clone()
        } else if !encryption_key.is_empty() {
            crate::crypto::decrypt(email_raw, encryption_key).unwrap_or_else(|_| email_raw.clone())
        } else {
            email_raw.clone()
        };

        let hash = crate::crypto::blind_index(&plaintext, encryption_key);

        let stored = if !encryption_key.is_empty() && plaintext.contains('@') {
            crate::crypto::encrypt(&plaintext, encryption_key)?
        } else {
            email_raw.clone()
        };

        sqlx::query(
            "UPDATE merchants SET recovery_email = ?, recovery_email_hash = ? WHERE id = ?",
        )
        .bind(&stored)
        .bind(&hash)
        .bind(id)
        .execute(pool)
        .await?;
    }
    tracing::info!("Recovery email encryption migration complete");
    Ok(())
}

/// Re-hash blind indices from plain SHA-256 to HMAC-SHA256 keyed with ENCRYPTION_KEY.
/// Detects old-format hashes by length (SHA-256 = 64 hex chars) and re-computes them.
/// Safe to run multiple times — skips rows that already have HMAC hashes.
pub async fn migrate_blind_index_to_hmac(
    pool: &SqlitePool,
    encryption_key: &str,
) -> anyhow::Result<()> {
    if encryption_key.is_empty() {
        return Ok(());
    }

    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, recovery_email, recovery_email_hash FROM merchants
         WHERE recovery_email IS NOT NULL AND recovery_email != ''
         AND recovery_email_hash IS NOT NULL AND recovery_email_hash != ''",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    let mut migrated = 0u32;
    for (id, enc_email, existing_hash) in &rows {
        let plaintext = crate::crypto::decrypt_email(enc_email, encryption_key)
            .unwrap_or_else(|_| enc_email.clone());
        let new_hash = crate::crypto::blind_index(&plaintext, encryption_key);

        if existing_hash.as_deref() == Some(&new_hash) {
            continue;
        }

        sqlx::query("UPDATE merchants SET recovery_email_hash = ? WHERE id = ?")
            .bind(&new_hash)
            .bind(id)
            .execute(pool)
            .await?;
        migrated += 1;
    }

    if migrated > 0 {
        tracing::info!(count = migrated, "Migrated blind indices to HMAC-SHA256");
    }
    Ok(())
}

/// Encrypt any plaintext webhook secrets in the database. Called once at startup when
/// ENCRYPTION_KEY is set. Plaintext secrets are identified by their "whsec_" prefix.
pub async fn migrate_encrypt_webhook_secrets(
    pool: &SqlitePool,
    encryption_key: &str,
) -> anyhow::Result<()> {
    if encryption_key.is_empty() {
        return Ok(());
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, webhook_secret FROM merchants WHERE webhook_secret LIKE 'whsec_%'",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(
        count = rows.len(),
        "Encrypting plaintext webhook secrets at rest"
    );
    for (id, secret) in &rows {
        let encrypted = crate::crypto::encrypt(secret, encryption_key)?;
        sqlx::query("UPDATE merchants SET webhook_secret = ? WHERE id = ?")
            .bind(&encrypted)
            .bind(id)
            .execute(pool)
            .await?;
    }
    tracing::info!("Webhook secret encryption migration complete");
    Ok(())
}

/// Encrypt any plaintext viewing keys in the database. Called once at startup when
/// ENCRYPTION_KEY is set. Plaintext keys are identified by their "uview"/"utest"/"uivk"/"uivktest" prefix.
pub async fn migrate_encrypt_ufvks(pool: &SqlitePool, encryption_key: &str) -> anyhow::Result<()> {
    if encryption_key.is_empty() {
        return Ok(());
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, ufvk FROM merchants WHERE ufvk LIKE 'uview%' OR ufvk LIKE 'utest%' OR ufvk LIKE 'uivk%'",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(
        count = rows.len(),
        "Encrypting plaintext viewing keys at rest"
    );
    for (id, key) in &rows {
        let encrypted = crate::crypto::encrypt(key, encryption_key)?;
        sqlx::query("UPDATE merchants SET ufvk = ? WHERE id = ?")
            .bind(&encrypted)
            .bind(id)
            .execute(pool)
            .await?;
    }
    tracing::info!("Viewing key encryption migration complete");
    Ok(())
}

/// Convert stored UFVKs to UIVKs. Called once at startup.
/// Decrypts each merchant's viewing key, and if it's still a UFVK (uview/uviewtest),
/// derives the UIVK and re-encrypts it. Idempotent: keys already stored as UIVK are skipped.
pub async fn migrate_ufvk_to_uivk(pool: &SqlitePool, encryption_key: &str) -> anyhow::Result<()> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, ufvk FROM merchants")
        .fetch_all(pool)
        .await?;

    if rows.is_empty() {
        return Ok(());
    }

    let mut converted = 0u32;
    let mut skipped = 0u32;

    for (id, stored_key) in &rows {
        let plaintext = crate::crypto::decrypt_or_plaintext(stored_key, encryption_key)?;

        if plaintext.starts_with("uivk") {
            skipped += 1;
            tracing::debug!(merchant_id = %id, "Already UIVK, skipping migration");
            continue;
        }

        if !plaintext.starts_with("uview") && !plaintext.starts_with("utest") {
            tracing::warn!(merchant_id = %id, "Unrecognized viewing key format, skipping");
            continue;
        }

        match crate::scanner::decrypt::derive_uivk_from_ufvk(&plaintext) {
            Ok(uivk) => {
                let new_stored = if encryption_key.is_empty() {
                    uivk
                } else {
                    crate::crypto::encrypt(&uivk, encryption_key)?
                };

                sqlx::query("UPDATE merchants SET ufvk = ? WHERE id = ?")
                    .bind(&new_stored)
                    .bind(id)
                    .execute(pool)
                    .await?;

                converted += 1;
                tracing::info!(merchant_id = %id, "Migrated UFVK → UIVK");
            }
            Err(e) => {
                tracing::error!(merchant_id = %id, error = %e, "Failed to derive UIVK from UFVK, skipping");
            }
        }
    }

    if converted > 0 {
        tracing::info!(converted, skipped, "UFVK → UIVK migration complete");
    } else {
        tracing::debug!(skipped = rows.len(), "No UFVK → UIVK migrations needed");
    }
    Ok(())
}
