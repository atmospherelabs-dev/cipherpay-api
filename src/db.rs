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

    // Disable FK checks during table-rename migrations so SQLite doesn't
    // auto-rewrite FK references in other tables (webhook_deliveries, fee_ledger).
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.ok();

    let needs_migrate: bool = sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='invoices'
         AND sql LIKE '%CHECK%' AND (sql NOT LIKE '%refunded%' OR sql LIKE '%shipped%')"
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0) > 0;

    if needs_migrate {
        tracing::info!("Migrating invoices table (removing shipped status)...");
        sqlx::query("UPDATE invoices SET status = 'confirmed' WHERE status = 'shipped'")
            .execute(&pool).await.ok();
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
                refund_address TEXT,
                status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'detected', 'confirmed', 'expired', 'refunded')),
                detected_txid TEXT,
                detected_at TEXT,
                confirmed_at TEXT,
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
                payment_address, zcash_uri, refund_address, status, detected_txid, detected_at,
                confirmed_at, refunded_at, expires_at, purge_after, created_at
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

    // Diversified addresses: per-invoice unique address derivation
    sqlx::query("ALTER TABLE merchants ADD COLUMN diversifier_index INTEGER NOT NULL DEFAULT 0")
        .execute(&pool).await.ok();
    sqlx::query("ALTER TABLE invoices ADD COLUMN diversifier_index INTEGER")
        .execute(&pool).await.ok();
    sqlx::query("ALTER TABLE invoices ADD COLUMN orchard_receiver_hex TEXT")
        .execute(&pool).await.ok();
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_orchard_receiver ON invoices(orchard_receiver_hex)")
        .execute(&pool).await.ok();

    // Underpayment/overpayment: zatoshi-based amount tracking
    sqlx::query("ALTER TABLE invoices ADD COLUMN price_zatoshis INTEGER NOT NULL DEFAULT 0")
        .execute(&pool).await.ok();
    sqlx::query("ALTER TABLE invoices ADD COLUMN received_zatoshis INTEGER NOT NULL DEFAULT 0")
        .execute(&pool).await.ok();
    sqlx::query("UPDATE invoices SET price_zatoshis = CAST(price_zec * 100000000 AS INTEGER) WHERE price_zatoshis = 0 AND price_zec > 0")
        .execute(&pool).await.ok();

    // Add 'underpaid' to status CHECK -- requires table recreation in SQLite
    let needs_underpaid: bool = sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='invoices'
         AND sql LIKE '%CHECK%' AND sql NOT LIKE '%underpaid%'"
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0) > 0;

    if needs_underpaid {
        tracing::info!("Migrating invoices table (adding underpaid status)...");
        sqlx::query("ALTER TABLE invoices RENAME TO invoices_old2")
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
                refund_address TEXT,
                status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'underpaid', 'detected', 'confirmed', 'expired', 'refunded')),
                detected_txid TEXT,
                detected_at TEXT,
                confirmed_at TEXT,
                refunded_at TEXT,
                expires_at TEXT NOT NULL,
                purge_after TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                diversifier_index INTEGER,
                orchard_receiver_hex TEXT,
                price_zatoshis INTEGER NOT NULL DEFAULT 0,
                received_zatoshis INTEGER NOT NULL DEFAULT 0
            )"
        ).execute(&pool).await.ok();
        sqlx::query(
            "INSERT INTO invoices SELECT
                id, merchant_id, memo_code, product_id, product_name, size,
                price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
                payment_address, zcash_uri, refund_address, status, detected_txid, detected_at,
                confirmed_at, refunded_at, expires_at, purge_after, created_at,
                diversifier_index, orchard_receiver_hex, price_zatoshis, received_zatoshis
             FROM invoices_old2"
        ).execute(&pool).await.ok();
        sqlx::query("DROP TABLE invoices_old2").execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status)")
            .execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_memo ON invoices(memo_code)")
            .execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_orchard_receiver ON invoices(orchard_receiver_hex)")
            .execute(&pool).await.ok();
        tracing::info!("Invoices table migration (underpaid) complete");
    }

    // Clean up leftover temp tables from migrations
    sqlx::query("DROP TABLE IF EXISTS invoices_old").execute(&pool).await.ok();
    sqlx::query("DROP TABLE IF EXISTS invoices_old2").execute(&pool).await.ok();

    // Repair FK references in webhook_deliveries/fee_ledger that may have been
    // auto-rewritten by SQLite during RENAME TABLE (pointing to invoices_old).
    let wd_schema: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='webhook_deliveries'"
    ).fetch_optional(&pool).await.ok().flatten();
    if let Some(ref schema) = wd_schema {
        if schema.contains("invoices_old") {
            tracing::info!("Repairing webhook_deliveries FK references...");
            sqlx::query("ALTER TABLE webhook_deliveries RENAME TO _wd_repair")
                .execute(&pool).await.ok();
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS webhook_deliveries (
                    id TEXT PRIMARY KEY,
                    invoice_id TEXT NOT NULL REFERENCES invoices(id),
                    url TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'delivered', 'failed')),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_attempt_at TEXT,
                    next_retry_at TEXT,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
                )"
            ).execute(&pool).await.ok();
            sqlx::query("INSERT OR IGNORE INTO webhook_deliveries SELECT * FROM _wd_repair")
                .execute(&pool).await.ok();
            sqlx::query("DROP TABLE _wd_repair").execute(&pool).await.ok();
            tracing::info!("webhook_deliveries FK repair complete");
        }
    }

    let fl_schema: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='fee_ledger'"
    ).fetch_optional(&pool).await.ok().flatten();
    if let Some(ref schema) = fl_schema {
        if schema.contains("invoices_old") {
            tracing::info!("Repairing fee_ledger FK references...");
            sqlx::query("ALTER TABLE fee_ledger RENAME TO _fl_repair")
                .execute(&pool).await.ok();
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS fee_ledger (
                    id TEXT PRIMARY KEY,
                    invoice_id TEXT NOT NULL REFERENCES invoices(id),
                    merchant_id TEXT NOT NULL REFERENCES merchants(id),
                    fee_amount_zec REAL NOT NULL,
                    auto_collected INTEGER NOT NULL DEFAULT 0,
                    collected_at TEXT,
                    billing_cycle_id TEXT,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
                )"
            ).execute(&pool).await.ok();
            sqlx::query("INSERT OR IGNORE INTO fee_ledger SELECT * FROM _fl_repair")
                .execute(&pool).await.ok();
            sqlx::query("DROP TABLE _fl_repair").execute(&pool).await.ok();
            tracing::info!("fee_ledger FK repair complete");
        }
    }

    // Re-enable FK enforcement after all migrations
    sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.ok();

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

    // Billing: merchant columns
    let billing_upgrades = [
        "ALTER TABLE merchants ADD COLUMN trust_tier TEXT NOT NULL DEFAULT 'new'",
        "ALTER TABLE merchants ADD COLUMN billing_status TEXT NOT NULL DEFAULT 'active'",
        "ALTER TABLE merchants ADD COLUMN billing_started_at TEXT",
    ];
    for sql in &billing_upgrades {
        sqlx::query(sql).execute(&pool).await.ok();
    }

    // Fee ledger
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS fee_ledger (
            id TEXT PRIMARY KEY,
            invoice_id TEXT NOT NULL REFERENCES invoices(id),
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            fee_amount_zec REAL NOT NULL,
            auto_collected INTEGER NOT NULL DEFAULT 0,
            collected_at TEXT,
            billing_cycle_id TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_fee_ledger_merchant ON fee_ledger(merchant_id)")
        .execute(&pool).await.ok();
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_fee_ledger_cycle ON fee_ledger(billing_cycle_id)")
        .execute(&pool).await.ok();
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_fee_ledger_invoice ON fee_ledger(invoice_id)")
        .execute(&pool).await.ok();

    // Billing cycles
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS billing_cycles (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            period_start TEXT NOT NULL,
            period_end TEXT NOT NULL,
            total_fees_zec REAL NOT NULL DEFAULT 0.0,
            auto_collected_zec REAL NOT NULL DEFAULT 0.0,
            outstanding_zec REAL NOT NULL DEFAULT 0.0,
            settlement_invoice_id TEXT,
            status TEXT NOT NULL DEFAULT 'open'
                CHECK (status IN ('open', 'invoiced', 'paid', 'past_due', 'suspended')),
            grace_until TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_billing_cycles_merchant ON billing_cycles(merchant_id)")
        .execute(&pool).await.ok();

    // Scanner state persistence (crash-safe block height tracking)
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS scanner_state (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )"
    )
    .execute(&pool)
    .await
    .ok();

    tracing::info!("Database ready (SQLite)");
    Ok(pool)
}

pub async fn get_scanner_state(pool: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT value FROM scanner_state WHERE key = ?"
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

pub async fn set_scanner_state(pool: &SqlitePool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO scanner_state (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Encrypt any plaintext UFVKs in the database. Called once at startup when
/// ENCRYPTION_KEY is set. Plaintext UFVKs are identified by their "uview"/"utest" prefix.
pub async fn migrate_encrypt_ufvks(pool: &SqlitePool, encryption_key: &str) -> anyhow::Result<()> {
    if encryption_key.is_empty() {
        return Ok(());
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, ufvk FROM merchants WHERE ufvk LIKE 'uview%' OR ufvk LIKE 'utest%'"
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(count = rows.len(), "Encrypting plaintext UFVKs at rest");
    for (id, ufvk) in &rows {
        let encrypted = crate::crypto::encrypt(ufvk, encryption_key)?;
        sqlx::query("UPDATE merchants SET ufvk = ? WHERE id = ?")
            .bind(&encrypted)
            .bind(id)
            .execute(pool)
            .await?;
    }
    tracing::info!("UFVK encryption migration complete");
    Ok(())
}
