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

    // Products table (pricing is handled by the prices table via default_price_id)
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS products (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            slug TEXT NOT NULL DEFAULT '',
            name TEXT NOT NULL,
            description TEXT,
            default_price_id TEXT,
            metadata TEXT,
            active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_products_merchant ON products(merchant_id)")
        .execute(&pool)
        .await
        .ok();

    // Drop legacy UNIQUE constraint on slug (slug is now cosmetic, product ID is the identifier)
    sqlx::query("DROP INDEX IF EXISTS sqlite_autoindex_products_1")
        .execute(&pool).await.ok();
    sqlx::query("DROP INDEX IF EXISTS idx_products_slug")
        .execute(&pool).await.ok();

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

    sqlx::query("ALTER TABLE invoices ADD COLUMN refund_txid TEXT")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE products ADD COLUMN default_price_id TEXT")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE products ADD COLUMN metadata TEXT")
        .execute(&pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE invoices ADD COLUMN currency TEXT")
        .execute(&pool)
        .await
        .ok();

    // Disable FK checks and prevent SQLite from auto-rewriting FK references
    // in other tables during ALTER TABLE RENAME (requires legacy_alter_table).
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.ok();
    sqlx::query("PRAGMA legacy_alter_table = ON").execute(&pool).await.ok();

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

    // Drop legacy price_eur/currency columns from products (moved to prices table)
    let products_has_price_eur: bool = sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM pragma_table_info('products') WHERE name = 'price_eur'"
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0) > 0;

    if products_has_price_eur {
        tracing::info!("Migrating products table (dropping legacy price_eur/currency columns)...");
        sqlx::query("ALTER TABLE products RENAME TO products_old")
            .execute(&pool).await.ok();
        sqlx::query(
            "CREATE TABLE products (
                id TEXT PRIMARY KEY,
                merchant_id TEXT NOT NULL REFERENCES merchants(id),
                slug TEXT NOT NULL DEFAULT '',
                name TEXT NOT NULL,
                description TEXT,
                default_price_id TEXT,
                metadata TEXT,
                active INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            )"
        ).execute(&pool).await.ok();
        sqlx::query(
            "INSERT INTO products (id, merchant_id, slug, name, description, default_price_id, metadata, active, created_at)
             SELECT id, merchant_id, slug, name, description, default_price_id, metadata, active, created_at
             FROM products_old"
        ).execute(&pool).await.ok();
        sqlx::query("DROP TABLE products_old").execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_products_merchant ON products(merchant_id)")
            .execute(&pool).await.ok();
        tracing::info!("Products table migration complete (price_eur/currency removed)");
    }
    sqlx::query("DROP TABLE IF EXISTS products_old").execute(&pool).await.ok();

    // Repair FK references in prices/invoices that may have been auto-rewritten
    // by SQLite during products RENAME TABLE (pointing to products_old).
    let prices_schema: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='prices'"
    ).fetch_optional(&pool).await.ok().flatten();
    if let Some(ref schema) = prices_schema {
        if schema.contains("products_old") {
            tracing::info!("Repairing prices table FK references...");
            sqlx::query("ALTER TABLE prices RENAME TO _prices_repair")
                .execute(&pool).await.ok();
            sqlx::query(
                "CREATE TABLE prices (
                    id TEXT PRIMARY KEY,
                    product_id TEXT NOT NULL REFERENCES products(id),
                    currency TEXT NOT NULL,
                    unit_amount REAL NOT NULL,
                    active INTEGER NOT NULL DEFAULT 1,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                    price_type TEXT NOT NULL DEFAULT 'one_time',
                    billing_interval TEXT,
                    interval_count INTEGER
                )"
            ).execute(&pool).await.ok();
            sqlx::query("INSERT OR IGNORE INTO prices SELECT * FROM _prices_repair")
                .execute(&pool).await.ok();
            sqlx::query("DROP TABLE _prices_repair").execute(&pool).await.ok();
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_prices_product ON prices(product_id)")
                .execute(&pool).await.ok();
            tracing::info!("prices FK repair complete");
        }
    }
    sqlx::query("DROP TABLE IF EXISTS _prices_repair").execute(&pool).await.ok();

    // Repair FK references in invoices if they point to products_old
    let inv_schema: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='invoices'"
    ).fetch_optional(&pool).await.ok().flatten();
    if let Some(ref schema) = inv_schema {
        if schema.contains("products_old") {
            tracing::info!("Repairing invoices table FK references (products_old)...");
            let inv_sql = schema.replace("products_old", "products");
            sqlx::query("ALTER TABLE invoices RENAME TO _inv_repair")
                .execute(&pool).await.ok();
            sqlx::query(&inv_sql).execute(&pool).await.ok();
            sqlx::query("INSERT OR IGNORE INTO invoices SELECT * FROM _inv_repair")
                .execute(&pool).await.ok();
            sqlx::query("DROP TABLE _inv_repair").execute(&pool).await.ok();
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status)")
                .execute(&pool).await.ok();
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_memo ON invoices(memo_code)")
                .execute(&pool).await.ok();
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_orchard_receiver ON invoices(orchard_receiver_hex)")
                .execute(&pool).await.ok();
            tracing::info!("invoices FK repair (products_old) complete");
        }
    }
    sqlx::query("DROP TABLE IF EXISTS _inv_repair").execute(&pool).await.ok();

    // Repair FK references in webhook_deliveries/fee_ledger that may have been
    // auto-rewritten by SQLite during RENAME TABLE. Check for all possible
    // dangling references: invoices_old, _inv_repair (from FK repair migration).
    // SQLite pool pragmas are per-connection, so legacy_alter_table may not have
    // been active on the connection that ran the rename.
    let wd_schema: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='webhook_deliveries'"
    ).fetch_optional(&pool).await.ok().flatten();
    if let Some(ref schema) = wd_schema {
        if schema.contains("invoices_old") || schema.contains("_inv_repair") {
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
    sqlx::query("DROP TABLE IF EXISTS _wd_repair").execute(&pool).await.ok();

    let fl_schema: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='fee_ledger'"
    ).fetch_optional(&pool).await.ok().flatten();
    if let Some(ref schema) = fl_schema {
        if schema.contains("invoices_old") || schema.contains("_inv_repair") {
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
    sqlx::query("DROP TABLE IF EXISTS _fl_repair").execute(&pool).await.ok();

    // Re-enable FK enforcement and restore default alter-table behavior
    sqlx::query("PRAGMA legacy_alter_table = OFF").execute(&pool).await.ok();
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
                CHECK (status IN ('open', 'invoiced', 'paid', 'past_due', 'suspended', 'carried_over')),
            grace_until TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_billing_cycles_merchant ON billing_cycles(merchant_id)")
        .execute(&pool).await.ok();

    // Migrate billing_cycles CHECK to include 'carried_over' for existing databases
    let bc_needs_migrate: bool = sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='billing_cycles'
         AND sql LIKE '%CHECK%' AND sql NOT LIKE '%carried_over%'"
    )
    .fetch_one(&pool).await.unwrap_or(0) > 0;

    if bc_needs_migrate {
        tracing::info!("Migrating billing_cycles table (adding carried_over status)...");
        sqlx::query("ALTER TABLE billing_cycles RENAME TO _bc_migrate")
            .execute(&pool).await.ok();
        sqlx::query(
            "CREATE TABLE billing_cycles (
                id TEXT PRIMARY KEY,
                merchant_id TEXT NOT NULL REFERENCES merchants(id),
                period_start TEXT NOT NULL,
                period_end TEXT NOT NULL,
                total_fees_zec REAL NOT NULL DEFAULT 0.0,
                auto_collected_zec REAL NOT NULL DEFAULT 0.0,
                outstanding_zec REAL NOT NULL DEFAULT 0.0,
                settlement_invoice_id TEXT,
                status TEXT NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'invoiced', 'paid', 'past_due', 'suspended', 'carried_over')),
                grace_until TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            )"
        ).execute(&pool).await.ok();
        sqlx::query("INSERT INTO billing_cycles SELECT * FROM _bc_migrate")
            .execute(&pool).await.ok();
        sqlx::query("DROP TABLE _bc_migrate").execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_billing_cycles_merchant ON billing_cycles(merchant_id)")
            .execute(&pool).await.ok();
        tracing::info!("billing_cycles migration complete");
    }

    // Prices table: separate pricing from products (Stripe pattern)
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS prices (
            id TEXT PRIMARY KEY,
            product_id TEXT NOT NULL REFERENCES products(id),
            currency TEXT NOT NULL,
            unit_amount REAL NOT NULL,
            active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_prices_product ON prices(product_id)")
        .execute(&pool).await.ok();

    // Optional ticketing metadata on prices (safe additive columns)
    for sql in &[
        "ALTER TABLE prices ADD COLUMN label TEXT",
        "ALTER TABLE prices ADD COLUMN max_quantity INTEGER",
    ] {
        sqlx::query(sql).execute(&pool).await.ok();
    }

    // Seed prices from existing products that don't have any prices yet (legacy: price_eur/currency)
    sqlx::query(
        "INSERT OR IGNORE INTO prices (id, product_id, currency, unit_amount)
         SELECT 'cprice_' || REPLACE(LOWER(HEX(RANDOMBLOB(16))), '-', ''),
                p.id, COALESCE(p.currency, 'EUR'), COALESCE(p.price_eur, 0)
         FROM products p
         WHERE NOT EXISTS (SELECT 1 FROM prices pr WHERE pr.product_id = p.id)
         AND (p.price_eur IS NOT NULL AND p.price_eur > 0)"
    )
    .execute(&pool)
    .await
    .ok();

    // Backfill default_price_id from first active price (for products migrated from legacy schema)
    sqlx::query(
        "UPDATE products SET default_price_id = (
            SELECT id FROM prices WHERE product_id = products.id AND active = 1 ORDER BY created_at ASC LIMIT 1
        ) WHERE default_price_id IS NULL AND EXISTS (SELECT 1 FROM prices pr WHERE pr.product_id = products.id)"
    )
    .execute(&pool)
    .await
    .ok();

    // Invoice schema additions for multi-currency pricing
    sqlx::query("ALTER TABLE invoices ADD COLUMN amount REAL")
        .execute(&pool).await.ok();
    sqlx::query("ALTER TABLE invoices ADD COLUMN price_id TEXT")
        .execute(&pool).await.ok();

    // Backfill amount from existing data
    sqlx::query(
        "UPDATE invoices SET amount = CASE
            WHEN currency = 'USD' THEN COALESCE(price_usd, price_eur)
            ELSE price_eur
         END
         WHERE amount IS NULL"
    )
    .execute(&pool)
    .await
    .ok();

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

    // x402 verification log
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS x402_verifications (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            txid TEXT NOT NULL,
            amount_zatoshis INTEGER,
            amount_zec REAL,
            status TEXT NOT NULL CHECK (status IN ('verified', 'rejected')),
            reason TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_x402_merchant ON x402_verifications(merchant_id, created_at)")
        .execute(&pool).await.ok();

    sqlx::query("ALTER TABLE x402_verifications ADD COLUMN protocol TEXT NOT NULL DEFAULT 'x402'")
        .execute(&pool).await.ok();

    // Sessions table (agentic prepaid credit)
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            deposit_txid TEXT NOT NULL,
            bearer_token TEXT NOT NULL UNIQUE,
            balance_zatoshis INTEGER NOT NULL,
            balance_remaining INTEGER NOT NULL,
            cost_per_request INTEGER NOT NULL DEFAULT 1000,
            requests_made INTEGER NOT NULL DEFAULT 0,
            refund_address TEXT,
            status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'closed', 'expired', 'depleted')),
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            closed_at TEXT
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_token ON sessions(bearer_token)")
        .execute(&pool).await.ok();
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_merchant ON sessions(merchant_id, status)")
        .execute(&pool).await.ok();

    // Price type columns (one_time vs recurring)
    for sql in &[
        "ALTER TABLE prices ADD COLUMN price_type TEXT NOT NULL DEFAULT 'one_time'",
        "ALTER TABLE prices ADD COLUMN billing_interval TEXT",
        "ALTER TABLE prices ADD COLUMN interval_count INTEGER",
    ] {
        sqlx::query(sql).execute(&pool).await.ok();
    }

    // Events are linked to products (composition model): billing stays product/price-based.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS events (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            product_id TEXT NOT NULL UNIQUE REFERENCES products(id),
            title TEXT NOT NULL,
            description TEXT,
            event_date TEXT,
            event_location TEXT,
            status TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('draft', 'active', 'cancelled', 'past')),
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_merchant_created ON events(merchant_id, created_at)")
        .execute(&pool)
        .await
        .ok();
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_status ON events(status)")
        .execute(&pool)
        .await
        .ok();

    // Ticket records are anchored to billing entities (invoice/product/price).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tickets (
            id TEXT PRIMARY KEY,
            invoice_id TEXT NOT NULL UNIQUE REFERENCES invoices(id),
            product_id TEXT NOT NULL REFERENCES products(id),
            price_id TEXT REFERENCES prices(id),
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            code TEXT NOT NULL UNIQUE,
            status TEXT NOT NULL DEFAULT 'valid'
                CHECK (status IN ('valid', 'used', 'void')),
            used_at TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    for sql in &[
        "CREATE INDEX IF NOT EXISTS idx_tickets_invoice ON tickets(invoice_id)",
        "CREATE INDEX IF NOT EXISTS idx_tickets_product_status ON tickets(product_id, status)",
        "CREATE INDEX IF NOT EXISTS idx_tickets_merchant_created ON tickets(merchant_id, created_at)",
    ] {
        sqlx::query(sql).execute(&pool).await.ok();
    }

    // Subscriptions: recurring invoice schedules (no customer data -- privacy first)
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS subscriptions (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL REFERENCES merchants(id),
            price_id TEXT NOT NULL REFERENCES prices(id),
            label TEXT,
            status TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active', 'past_due', 'canceled', 'paused')),
            current_period_start TEXT NOT NULL,
            current_period_end TEXT NOT NULL,
            cancel_at_period_end INTEGER NOT NULL DEFAULT 0,
            canceled_at TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )"
    )
    .execute(&pool)
    .await
    .ok();

    // Migration: add label column for existing databases
    sqlx::query("ALTER TABLE subscriptions ADD COLUMN label TEXT")
        .execute(&pool).await.ok();

    // Subscription lifecycle: link invoices to subscriptions
    sqlx::query("ALTER TABLE invoices ADD COLUMN subscription_id TEXT")
        .execute(&pool).await.ok();
    sqlx::query("ALTER TABLE subscriptions ADD COLUMN current_invoice_id TEXT")
        .execute(&pool).await.ok();

    // Add 'draft' to invoice status CHECK (for subscription pre-invoicing)
    let needs_draft: bool = sqlx::query_scalar::<_, i32>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='invoices'
         AND sql LIKE '%CHECK%' AND sql NOT LIKE '%draft%'"
    )
    .fetch_one(&pool).await.unwrap_or(0) > 0;

    if needs_draft {
        tracing::info!("Migrating invoices table (adding draft status)...");
        sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.ok();
        sqlx::query("PRAGMA legacy_alter_table = ON").execute(&pool).await.ok();
        sqlx::query("ALTER TABLE invoices RENAME TO _inv_draft_migrate")
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
                    CHECK (status IN ('draft', 'pending', 'underpaid', 'detected', 'confirmed', 'expired', 'refunded')),
                detected_txid TEXT,
                detected_at TEXT,
                confirmed_at TEXT,
                refunded_at TEXT,
                refund_txid TEXT,
                expires_at TEXT NOT NULL,
                purge_after TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                diversifier_index INTEGER,
                orchard_receiver_hex TEXT,
                price_zatoshis INTEGER NOT NULL DEFAULT 0,
                received_zatoshis INTEGER NOT NULL DEFAULT 0,
                amount REAL,
                price_id TEXT,
                subscription_id TEXT
            )"
        ).execute(&pool).await.ok();
        sqlx::query(
            "INSERT INTO invoices SELECT
                id, merchant_id, memo_code, product_id, product_name, size,
                price_eur, price_usd, currency, price_zec, zec_rate_at_creation,
                payment_address, zcash_uri, refund_address, status, detected_txid, detected_at,
                confirmed_at, refunded_at, refund_txid, expires_at, purge_after, created_at,
                diversifier_index, orchard_receiver_hex, price_zatoshis, received_zatoshis,
                amount, price_id, subscription_id
             FROM _inv_draft_migrate"
        ).execute(&pool).await.ok();
        sqlx::query("DROP TABLE _inv_draft_migrate").execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status)")
            .execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_memo ON invoices(memo_code)")
            .execute(&pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_invoices_orchard_receiver ON invoices(orchard_receiver_hex)")
            .execute(&pool).await.ok();
        sqlx::query("PRAGMA legacy_alter_table = OFF").execute(&pool).await.ok();
        sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.ok();
        tracing::info!("Invoices table migration (draft status) complete");
    }
    sqlx::query("DROP TABLE IF EXISTS _inv_draft_migrate").execute(&pool).await.ok();

    // Belt-and-suspenders: ensure subscription columns exist even if earlier
    // ALTER TABLEs failed silently due to pool-connection pragma issues
    for (table, col) in &[
        ("invoices", "subscription_id"),
        ("invoices", "amount"),
        ("invoices", "price_id"),
        ("subscriptions", "current_invoice_id"),
        ("subscriptions", "label"),
    ] {
        let exists: bool = sqlx::query_scalar::<_, i32>(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{}') WHERE name = '{}'", table, col)
        ).fetch_one(&pool).await.unwrap_or(0) > 0;
        if !exists {
            tracing::info!("Adding missing column {}.{}", table, col);
            sqlx::query(&format!("ALTER TABLE {} ADD COLUMN {} TEXT", table, col))
                .execute(&pool).await.ok();
        }
    }

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_subscriptions_merchant ON subscriptions(merchant_id)")
        .execute(&pool).await.ok();
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_subscriptions_status ON subscriptions(status)")
        .execute(&pool).await.ok();

    // Webhook delivery enrichment: queryable event_type, merchant_id, response tracking
    for sql in &[
        "ALTER TABLE webhook_deliveries ADD COLUMN event_type TEXT",
        "ALTER TABLE webhook_deliveries ADD COLUMN merchant_id TEXT",
        "ALTER TABLE webhook_deliveries ADD COLUMN response_status INTEGER",
        "ALTER TABLE webhook_deliveries ADD COLUMN response_error TEXT",
    ] {
        sqlx::query(sql).execute(&pool).await.ok();
    }

    // Recovery email encryption: add blind-index column
    sqlx::query("ALTER TABLE merchants ADD COLUMN recovery_email_hash TEXT")
        .execute(&pool).await.ok();
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_merchants_email_hash ON merchants(recovery_email_hash)")
        .execute(&pool).await.ok();

    // Luma integration: merchant API key, event/price linking, invoice PII + registration state
    for sql in &[
        "ALTER TABLE merchants ADD COLUMN luma_api_key TEXT",
        "ALTER TABLE events ADD COLUMN luma_event_id TEXT",
        "ALTER TABLE events ADD COLUMN luma_event_url TEXT",
        "ALTER TABLE prices ADD COLUMN luma_ticket_type_id TEXT",
        "ALTER TABLE invoices ADD COLUMN attendee_name TEXT",
        "ALTER TABLE invoices ADD COLUMN attendee_email TEXT",
        "ALTER TABLE invoices ADD COLUMN luma_registration_status TEXT",
        "ALTER TABLE invoices ADD COLUMN luma_guest_data TEXT",
        "ALTER TABLE invoices ADD COLUMN luma_retry_at TEXT",
        "ALTER TABLE invoices ADD COLUMN luma_retry_count INTEGER DEFAULT 0",
    ] {
        sqlx::query(sql).execute(&pool).await.ok();
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_luma ON events(luma_event_id)")
        .execute(&pool).await.ok();

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

/// Periodic data purge: cleans up expired sessions, old webhook deliveries,
/// expired recovery tokens, and optionally old expired/refunded invoices.
pub async fn run_data_purge(pool: &SqlitePool, purge_days: i64) -> anyhow::Result<()> {
    let cutoff = format!("-{} days", purge_days);

    // Expired sessions + closed/depleted sessions older than cutoff
    let sessions = sqlx::query(
        "DELETE FROM sessions WHERE
            expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            OR (status IN ('closed', 'depleted') AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?))"
    ).bind(&cutoff).execute(pool).await?;

    // Expired recovery tokens
    let tokens = sqlx::query(
        "DELETE FROM recovery_tokens WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    ).execute(pool).await?;

    // Old delivered/failed webhook deliveries
    let webhooks = sqlx::query(
        "DELETE FROM webhook_deliveries WHERE status IN ('delivered', 'failed')
         AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)"
    ).bind(&cutoff).execute(pool).await?;

    let tickets = sqlx::query(
        "DELETE FROM tickets
         WHERE status = 'void'
         AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)"
    ).bind(&cutoff).execute(pool).await?;

    let total = sessions.rows_affected()
        + tokens.rows_affected()
        + webhooks.rows_affected()
        + tickets.rows_affected();
    if total > 0 {
        tracing::info!(
            sessions = sessions.rows_affected(),
            tokens = tokens.rows_affected(),
            webhooks = webhooks.rows_affected(),
            tickets = tickets.rows_affected(),
            "Data purge completed"
        );
    }
    Ok(())
}

/// Encrypt any plaintext recovery emails and backfill blind-index hashes.
/// Called once at startup when ENCRYPTION_KEY is set.
/// Plaintext emails are identified by containing '@'.
pub async fn migrate_encrypt_recovery_emails(pool: &SqlitePool, encryption_key: &str) -> anyhow::Result<()> {
    // Backfill hashes for rows that have a recovery_email but no hash
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, recovery_email FROM merchants WHERE recovery_email IS NOT NULL AND recovery_email != '' AND (recovery_email_hash IS NULL OR recovery_email_hash = '')"
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(count = rows.len(), "Migrating recovery emails (encrypt + blind index)");
    for (id, email_raw) in &rows {
        let plaintext = if email_raw.contains('@') {
            email_raw.clone()
        } else if !encryption_key.is_empty() {
            crate::crypto::decrypt(email_raw, encryption_key).unwrap_or_else(|_| email_raw.clone())
        } else {
            email_raw.clone()
        };

        let hash = crate::crypto::blind_index(&plaintext);

        let stored = if !encryption_key.is_empty() && plaintext.contains('@') {
            crate::crypto::encrypt(&plaintext, encryption_key)?
        } else {
            email_raw.clone()
        };

        sqlx::query("UPDATE merchants SET recovery_email = ?, recovery_email_hash = ? WHERE id = ?")
            .bind(&stored)
            .bind(&hash)
            .bind(id)
            .execute(pool)
            .await?;
    }
    tracing::info!("Recovery email encryption migration complete");
    Ok(())
}

/// Encrypt any plaintext webhook secrets in the database. Called once at startup when
/// ENCRYPTION_KEY is set. Plaintext secrets are identified by their "whsec_" prefix.
pub async fn migrate_encrypt_webhook_secrets(pool: &SqlitePool, encryption_key: &str) -> anyhow::Result<()> {
    if encryption_key.is_empty() {
        return Ok(());
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, webhook_secret FROM merchants WHERE webhook_secret LIKE 'whsec_%'"
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(count = rows.len(), "Encrypting plaintext webhook secrets at rest");
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
        "SELECT id, ufvk FROM merchants WHERE ufvk LIKE 'uview%' OR ufvk LIKE 'utest%' OR ufvk LIKE 'uivk%'"
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    tracing::info!(count = rows.len(), "Encrypting plaintext viewing keys at rest");
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
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, ufvk FROM merchants"
    )
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
