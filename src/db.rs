use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

pub async fn create_pool(database_url: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    // Run migrations inline
    sqlx::query(include_str!("../migrations/001_init.sql"))
        .execute(&pool)
        .await
        .ok(); // Ignore if tables already exist

    // Schema upgrades for existing databases
    let upgrades = [
        "ALTER TABLE merchants ADD COLUMN webhook_secret TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE merchants ADD COLUMN dashboard_token_hash TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE merchants ADD COLUMN recovery_email TEXT",
    ];
    for sql in &upgrades {
        sqlx::query(sql).execute(&pool).await.ok();
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    // Add payment_address + zcash_uri to invoices for checkout display
    let invoice_upgrades = [
        "ALTER TABLE invoices ADD COLUMN payment_address TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE invoices ADD COLUMN zcash_uri TEXT NOT NULL DEFAULT ''",
    ];
    for sql in &invoice_upgrades {
        sqlx::query(sql).execute(&pool).await.ok();
    }

    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_merchants_ufvk ON merchants(ufvk)")
        .execute(&pool)
        .await
        .ok();

    tracing::info!("Database ready (SQLite)");
    Ok(pool)
}
