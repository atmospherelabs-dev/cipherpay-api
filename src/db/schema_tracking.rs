use sqlx::SqlitePool;

pub(crate) async fn ensure_migration_tracking_table(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            name TEXT PRIMARY KEY,
            status TEXT NOT NULL CHECK (status IN ('running', 'applied', 'failed')),
            started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            finished_at TEXT,
            error_message TEXT
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn run_tracked_migration<F, Fut>(
    pool: &SqlitePool,
    name: &str,
    migration: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    ensure_migration_tracking_table(pool).await?;

    let existing: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, error_message FROM schema_migrations WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await?;

    if let Some((status, _)) = &existing {
        if status == "applied" {
            tracing::info!(migration = name, "Migration already applied");
            return Ok(());
        }
        if status == "running" {
            anyhow::bail!(
                "Migration '{}' is marked as running from a previous startup. Inspect schema_migrations and the database before restarting again.",
                name
            );
        }
    }

    sqlx::query(
        "INSERT INTO schema_migrations (name, status, started_at, finished_at, error_message)
         VALUES (?, 'running', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), NULL, NULL)
         ON CONFLICT(name) DO UPDATE SET
            status = 'running',
            started_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
            finished_at = NULL,
            error_message = NULL",
    )
    .bind(name)
    .execute(pool)
    .await?;

    match migration().await {
        Ok(()) => {
            sqlx::query(
                "UPDATE schema_migrations
                 SET status = 'applied',
                     finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                     error_message = NULL
                 WHERE name = ?",
            )
            .bind(name)
            .execute(pool)
            .await?;
            tracing::info!(migration = name, "Migration applied successfully");
            Ok(())
        }
        Err(error) => {
            let error_text = error.to_string();
            sqlx::query(
                "UPDATE schema_migrations
                 SET status = 'failed',
                     finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                     error_message = ?
                 WHERE name = ?",
            )
            .bind(&error_text)
            .bind(name)
            .execute(pool)
            .await?;
            anyhow::bail!("Migration '{}' failed: {}", name, error_text);
        }
    }
}

pub(crate) async fn validate_schema_state(pool: &SqlitePool) -> anyhow::Result<()> {
    let required_tables = [
        "merchants",
        "invoices",
        "products",
        "prices",
        "webhook_deliveries",
        "fee_ledger",
        "billing_cycles",
        "x402_verifications",
        "agent_sessions",
        "session_requests",
        "recovery_tokens",
        "email_events",
    ];
    for table in required_tables {
        ensure_table_exists(pool, table).await?;
    }

    ensure_columns(
        pool,
        "merchants",
        &[
            "webhook_secret",
            "dashboard_token_hash",
            "recovery_email",
            "name",
            "diversifier_index",
            "trust_tier",
            "billing_status",
            "billing_started_at",
            "recovery_email_hash",
            "fee_rate",
            "fee_discount_until",
        ],
    )
    .await?;
    ensure_columns(
        pool,
        "invoices",
        &[
            "payment_address",
            "zcash_uri",
            "product_id",
            "refund_address",
            "price_usd",
            "refunded_at",
            "refund_txid",
            "currency",
            "diversifier_index",
            "orchard_receiver_hex",
            "price_zatoshis",
            "received_zatoshis",
        ],
    )
    .await?;
    ensure_columns(
        pool,
        "webhook_deliveries",
        &[
            "event_type",
            "merchant_id",
            "response_status",
            "response_error",
        ],
    )
    .await?;
    ensure_columns(
        pool,
        "fee_ledger",
        &["fee_amount_zatoshis", "fee_rate_applied"],
    )
    .await?;
    ensure_columns(
        pool,
        "billing_cycles",
        &[
            "total_fees_zatoshis",
            "auto_collected_zatoshis",
            "outstanding_zatoshis",
        ],
    )
    .await?;
    ensure_columns(pool, "x402_verifications", &["protocol"]).await?;

    let webhook_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='webhook_deliveries'",
    )
    .fetch_one(pool)
    .await?;
    if webhook_sql.contains("invoice_id TEXT NOT NULL") {
        anyhow::bail!("webhook_deliveries.invoice_id is still NOT NULL after migration");
    }

    Ok(())
}

async fn ensure_table_exists(pool: &SqlitePool, table: &str) -> anyhow::Result<()> {
    let exists: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND name = ?")
            .bind(table)
            .fetch_optional(pool)
            .await?;

    if exists.is_none() {
        anyhow::bail!("Required table '{}' is missing after migrations", table);
    }

    Ok(())
}

async fn ensure_columns(pool: &SqlitePool, table: &str, required: &[&str]) -> anyhow::Result<()> {
    let pragma = format!("PRAGMA table_info({})", table);
    let rows: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&pragma).fetch_all(pool).await?;
    let existing: std::collections::HashSet<String> = rows.into_iter().map(|row| row.1).collect();

    for column in required {
        if !existing.contains(*column) {
            anyhow::bail!(
                "Required column '{}.{}' is missing after migrations",
                table,
                column
            );
        }
    }

    Ok(())
}
