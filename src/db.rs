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
        "ALTER TABLE merchants ADD COLUMN name TEXT NOT NULL DEFAULT ''",
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

    // Products table for existing databases
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS products (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            slug TEXT NOT NULL,
            name TEXT NOT NULL,
            description TEXT,
            price_eur REAL NOT NULL,
            variants TEXT,
            active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            UNIQUE(merchant_id, slug)
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_products_merchant ON products(merchant_id)")
        .execute(&pool)
        .await
        .ok();

    // Add product_id and refund_address to invoices for existing databases
    sqlx::query("ALTER TABLE invoices ADD COLUMN product_id TEXT REFERENCES products(id)")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE invoices ADD COLUMN refund_address TEXT")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE invoices ADD COLUMN price_usd REAL")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE invoices ADD COLUMN refunded_at TEXT")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE products ADD COLUMN currency TEXT NOT NULL DEFAULT 'EUR'")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE invoices ADD COLUMN currency TEXT")
        .execute(&pool)
        .await
        .ok();

    // Remove old CHECK constraint on invoices.status to allow 'refunded'
    // SQLite doesn't support ALTER CONSTRAINT, so we check if the constraint blocks us
    // and recreate the table if needed.
    let needs_migrate: bool = sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='invoices'
         AND sql LIKE '%CHECK%' AND sql NOT LIKE '%refunded%'"
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0) > 0;

    if needs_migrate {
        tracing::info!("Migrating invoices table to add 'refunded' status...");
        sqlx::query("ALTER TABLE invoices RENAME TO invoices_old")
            .execute(&pool).await.ok();
        sqlx::query(
            "CREATE TABLE invoices (
                id TEXT PRIMARY KEY,
                merchant_id TEXT NOT NULL REFERENCES merchants(id),
                memo_code TEXT NOT NULL UNIQUE,
                product_id TEXT REFERENCES products(id),
                product_name TEXT,
                size TEXT,
                price_eur REAL NOT NULL,
                price_usd REAL,
                currency TEXT,
                price_zec REAL NOT NULL,
                zec_rate_at_creation REAL NOT NULL,
                payment_address TEXT NOT NULL DEFAULT '',
                zcash_uri TEXT NOT NULL DEFAULT '',
                shipping_alias TEXT,
                shipping_address TEXT,
                shipping_region TEXT,
                refund_address TEXT,
                status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'detected', 'confirmed', 'expired', 'shipped', 'refunded')),
                detected_txid TEXT,
                detected_at TEXT,
                confirmed_at TEXT,
                shipped_at TEXT,
                refunded_at TEXT,
                expires_at TEXT NOT NULL,
                purge_after TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            )"
        ).execute(&pool).await.ok();
        sqlx::query(
            "INSERT INTO invoices SELECT
                id, merchant_id, memo_code, product_id, product_name, size,
                price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
                payment_address, zcash_uri, shipping_alias, shipping_address,
                shipping_region, refund_address, status, detected_txid, detected_at,
                confirmed_at, shipped_at, refunded_at, expires_at, purge_after, created_at
             FROM invoices_old"
        ).execute(&pool).await.ok();
        sqlx::query("DROP TABLE invoices_old").execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status)")
            .execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_memo ON invoices(memo_code)")
            .execute(&pool).await.ok();
        tracing::info!("Invoices table migration complete");
    }

    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_merchants_ufvk ON merchants(ufvk)")
        .execute(&pool)
        .await
        .ok();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS recovery_tokens (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            token_hash TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    tracing::info!("Database ready (SQLite)");
    Ok(pool)
}
