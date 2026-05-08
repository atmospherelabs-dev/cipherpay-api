mod bootstrap;
mod data_migrations;
mod maintenance;
mod scanner_state;
mod schema_tracking;

use bootstrap::apply_inline_schema_migration;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

pub use data_migrations::{
    migrate_blind_index_to_hmac, migrate_encrypt_recovery_emails, migrate_encrypt_ufvks,
    migrate_encrypt_webhook_secrets, migrate_ufvk_to_uivk,
};
pub use maintenance::run_data_purge;
pub use scanner_state::{get_scanner_state, set_scanner_state};
use schema_tracking::ensure_migration_tracking_table;
pub use schema_tracking::run_tracked_migration;

pub async fn create_pool(database_url: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    ensure_migration_tracking_table(&pool).await?;
    run_tracked_migration(&pool, "schema_inline_v2026_04_07", || {
        let pool = pool.clone();
        async move { apply_inline_schema_migration(pool).await }
    })
    .await?;

    // Billing email infrastructure: per-merchant fee rate, discount expiry, email idempotency
    run_tracked_migration(&pool, "billing_emails_v2026_04_10", || async {
        for sql in &[
            "ALTER TABLE merchants ADD COLUMN fee_rate REAL",
            "ALTER TABLE merchants ADD COLUMN fee_discount_until TEXT",
            "ALTER TABLE fee_ledger ADD COLUMN fee_rate_applied REAL",
        ] {
            sqlx::query(sql).execute(&pool).await.ok();
        }

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS email_events (
                id TEXT PRIMARY KEY,
                merchant_id TEXT NOT NULL,
                template TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                sent_at TEXT NOT NULL,
                UNIQUE(merchant_id, template, entity_id)
            )",
        )
        .execute(&pool)
        .await
        .ok();

        Ok(())
    })
    .await?;

    run_tracked_migration(&pool, "passkey_auth_v2026_04_20", || async {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS passkey_credentials (
                id TEXT PRIMARY KEY,
                merchant_id TEXT NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                credential_json TEXT NOT NULL,
                label TEXT NOT NULL DEFAULT '',
                last_used_at TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            )",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_passkey_creds_merchant ON passkey_credentials(merchant_id)",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS passkey_challenges (
                id TEXT PRIMARY KEY,
                merchant_id TEXT,
                flow_type TEXT NOT NULL CHECK(flow_type IN ('register', 'login')),
                state_json TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            )",
        )
        .execute(&pool)
        .await
        .ok();

        for sql in &[
            "ALTER TABLE sessions ADD COLUMN reauth_at TEXT",
            "ALTER TABLE merchants ADD COLUMN last_token_login_at TEXT",
        ] {
            sqlx::query(sql).execute(&pool).await.ok();
        }

        Ok(())
    })
    .await?;

    // Drop the deposit-gated programmatic registration scaffolding. The flow was
    // never deployed; this migration only affects local dev DBs that ran the
    // earlier programmatic_registration_v2026_04_28 migration. Production never
    // had it, so this is a no-op there. SQLite's DROP COLUMN requires sqlite >= 3.35.
    run_tracked_migration(&pool, "remove_programmatic_registration_v2026_05_01", || async {
        sqlx::query("DROP TABLE IF EXISTS registration_requests")
            .execute(&pool)
            .await
            .ok();
        sqlx::query("ALTER TABLE merchants DROP COLUMN merchant_type")
            .execute(&pool)
            .await
            .ok();
        Ok(())
    })
    .await?;

    // Restricted API keys: per-merchant scoped credentials. A row with key_type='full'
    // grants the same access as the legacy merchants.api_key_hash column (which we
    // keep for backwards compat). A row with key_type='restricted' is rejected on a
    // small deny-list of account-management endpoints (see crate::api_keys::scope).
    run_tracked_migration(&pool, "merchant_api_keys_v2026_05_01", || async {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS merchant_api_keys (
                id TEXT PRIMARY KEY,
                merchant_id TEXT NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                key_hash TEXT NOT NULL UNIQUE,
                key_prefix TEXT NOT NULL,
                key_type TEXT NOT NULL CHECK (key_type IN ('full', 'restricted')),
                label TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                last_used_at TEXT,
                revoked_at TEXT
            )",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_merchant_api_keys_hash_active
             ON merchant_api_keys(key_hash) WHERE revoked_at IS NULL",
        )
        .execute(&pool)
        .await
        .ok();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_merchant_api_keys_merchant
             ON merchant_api_keys(merchant_id)",
        )
        .execute(&pool)
        .await
        .ok();

        Ok(())
    })
    .await?;

    // Subscription pause/resume: add flag to schedule pause at period end.
    // The 'paused' status already exists in the subscriptions CHECK constraint.
    run_tracked_migration(&pool, "subscription_pause_v2026_05_08", || async {
        sqlx::query(
            "ALTER TABLE subscriptions ADD COLUMN pause_at_period_end INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await
        .ok();
        Ok(())
    })
    .await?;

    Ok(pool)
}
