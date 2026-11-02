//! Minimal repeat-referral protection, independent of account deletion.
//! Wallet fingerprints identify a known receiver, not a person or business.
use anyhow::{ensure, Context};
use sqlx::{Sqlite, SqlitePool, Transaction};

#[derive(Debug)]
pub struct PendingAccountFees;
impl std::fmt::Display for PendingAccountFees {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Settle outstanding fees and wait for pending payment fees to be verified before deleting your account.")
    }
}
impl std::error::Error for PendingAccountFees {}

pub(crate) async fn check_deletion(
    tx: &mut Transaction<'_, Sqlite>,
    merchant: &str,
) -> anyhow::Result<()> {
    let pending: bool = sqlx::query_scalar("SELECT
        EXISTS(SELECT 1 FROM fee_ledger f LEFT JOIN billing_cycles b ON b.id=f.billing_cycle_id
          WHERE f.merchant_id=? AND f.fee_amount_zatoshis>f.collected_zatoshis AND COALESCE(b.status,'')!='paid')
        OR EXISTS(SELECT 1 FROM deferred_payment_fees WHERE merchant_id=? AND state='verifying')
        OR EXISTS(SELECT 1 FROM crosschain_fees f JOIN invoices i ON i.id=f.invoice_id
          WHERE i.merchant_id=? AND f.state NOT IN ('collected','direct','provider_handled') AND i.status NOT IN ('expired','cancelled','refunded'))")
        .bind(merchant).bind(merchant).bind(merchant).fetch_one(&mut **tx).await?;
    if pending {
        return Err(PendingAccountFees.into());
    }
    Ok(())
}

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let trigger: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='invoice_fee_policy_snapshot'",
    )
    .fetch_one(&mut *tx)
    .await?;
    // Historical actor/invoice IDs in the reward ledger must survive account
    // deletion. They confer no authentication rights and contain no wallet data.
    sqlx::raw_sql(
        "DROP TRIGGER invoice_fee_policy_snapshot;
      CREATE TABLE merchant_referrals_new (
        merchant_id TEXT PRIMARY KEY REFERENCES merchants(id) ON DELETE CASCADE,
        referrer_id TEXT NOT NULL,
        applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
        discount_until TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now','+3 months')),
        earning_until TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now','+12 months')),
        CHECK (merchant_id != referrer_id));
      INSERT INTO merchant_referrals_new SELECT * FROM merchant_referrals;
      DROP TABLE merchant_referrals;
      ALTER TABLE merchant_referrals_new RENAME TO merchant_referrals;
      CREATE TABLE invoice_fee_policy_new (
        invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
        service_bps INTEGER NOT NULL CHECK(service_bps BETWEEN 0 AND 1000),
        referrer_id TEXT);
      INSERT INTO invoice_fee_policy_new SELECT * FROM invoice_fee_policy;
      DROP TABLE invoice_fee_policy;
      ALTER TABLE invoice_fee_policy_new RENAME TO invoice_fee_policy;
      CREATE TABLE referral_earnings_new (
        receipt_id TEXT PRIMARY KEY, invoice_id TEXT NOT NULL,
        referrer_id TEXT NOT NULL, asset_id TEXT NOT NULL,
        collected_units TEXT NOT NULL, reward_units TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
      INSERT INTO referral_earnings_new SELECT * FROM referral_earnings;
      DROP TABLE referral_earnings;
      ALTER TABLE referral_earnings_new RENAME TO referral_earnings;
      CREATE INDEX referral_earnings_owner ON referral_earnings(referrer_id);
      CREATE TABLE direct_fee_receipts_new (
        invoice_id TEXT NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
        txid TEXT NOT NULL, units INTEGER NOT NULL CHECK(units>0),
        PRIMARY KEY(invoice_id,txid));
      INSERT INTO direct_fee_receipts_new SELECT * FROM direct_fee_receipts;
      DROP TABLE direct_fee_receipts;
      ALTER TABLE direct_fee_receipts_new RENAME TO direct_fee_receipts;
      CREATE TABLE referral_wallet_history (
        fingerprint TEXT PRIMARY KEY, merchant_id TEXT NOT NULL,
        referrer_id TEXT NOT NULL, applied_at TEXT NOT NULL,
        discount_until TEXT NOT NULL, earning_until TEXT NOT NULL);",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(&trigger).execute(&mut *tx).await?;
    let existing: Vec<String> = sqlx::query_scalar("SELECT merchant_id FROM merchant_referrals")
        .fetch_all(&mut *tx)
        .await?;
    for merchant in existing {
        remember(&mut tx, &merchant).await?;
    }
    tx.commit().await?;
    Ok(())
}

fn fingerprint(address: &str) -> String {
    crate::merchants::hash_key(&format!("cipherpay:referral-wallet:v1:{address}"))
}

pub(super) async fn check(
    tx: &mut Transaction<'_, Sqlite>,
    merchant: &str,
    referrer: &str,
) -> anyhow::Result<()> {
    let address: String = sqlx::query_scalar("SELECT payment_address FROM merchants WHERE id=?")
        .bind(merchant)
        .fetch_one(&mut **tx)
        .await?;
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM referral_wallet_history WHERE fingerprint=?)",
    )
    .bind(fingerprint(&address))
    .fetch_one(&mut **tx)
    .await?;
    ensure!(!used, "This wallet has already used a referral. Creating another account does not restart the offer.");
    let cycle: bool = sqlx::query_scalar("WITH RECURSIVE parents(id) AS (
        SELECT ? UNION SELECT r.referrer_id FROM merchant_referrals r JOIN parents p ON r.merchant_id=p.id
      ) SELECT EXISTS(SELECT 1 FROM parents WHERE id=?)")
        .bind(referrer).bind(merchant).fetch_one(&mut **tx).await?;
    ensure!(
        !cycle,
        "Merchants cannot refer each other in a referral loop."
    );
    Ok(())
}

/// Remember both the receiver at application and, defensively, at deletion.
/// Only a one-way fingerprint and original eligibility dates are retained.
pub(crate) async fn remember(
    tx: &mut Transaction<'_, Sqlite>,
    merchant: &str,
) -> anyhow::Result<()> {
    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT m.payment_address,r.referrer_id,r.applied_at,r.discount_until,r.earning_until
         FROM merchants m JOIN merchant_referrals r ON r.merchant_id=m.id WHERE m.id=?",
    )
    .bind(merchant)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((address, referrer, applied, discount, earning)) = row {
        ensure!(!address.is_empty(), "Missing referral wallet");
        sqlx::query("INSERT INTO referral_wallet_history VALUES (?,?,?,?,?,?) ON CONFLICT(fingerprint) DO NOTHING")
            .bind(fingerprint(&address)).bind(merchant).bind(referrer)
            .bind(applied).bind(discount).bind(earning)
            .execute(&mut **tx).await.context("Could not retain referral eligibility")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_preserves_existing_dates_rewards_and_fee_receipts() {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(
            "PRAGMA foreign_keys=ON;
          CREATE TABLE merchants(id TEXT PRIMARY KEY,payment_address TEXT,fee_rate REAL);
          CREATE TABLE invoices(id TEXT PRIMARY KEY,merchant_id TEXT,created_at TEXT,status TEXT);
          CREATE TABLE fee_ledger(auto_collected INTEGER,fee_amount_zatoshis INTEGER);
          INSERT INTO merchants VALUES ('a','wallet-a',NULL),('b','wallet-b',NULL);",
        )
        .execute(&p)
        .await
        .unwrap();
        crate::referrals::migrate(&p).await.unwrap();
        sqlx::raw_sql("INSERT INTO merchant_referrals VALUES ('b','a','2026-01-01T00:00:00Z','2026-04-01T00:00:00Z','2027-01-01T00:00:00Z');
          INSERT INTO invoices VALUES ('i','b','2026-02-01T00:00:00Z','confirmed');
          INSERT INTO referral_earnings VALUES ('r','i','a','zec','100','50','2026-02-02T00:00:00Z');
          INSERT INTO direct_fee_receipts VALUES ('i','tx',100);")
            .execute(&p).await.unwrap();
        migrate(&p).await.unwrap();
        assert_eq!(crate::referrals::service_bps(&p, "i").await.unwrap(), 50);
        let row: (String, String, String) = sqlx::query_as(
            "SELECT applied_at,discount_until,earning_until FROM referral_wallet_history",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "2026-01-01T00:00:00Z".into(),
                "2026-04-01T00:00:00Z".into(),
                "2027-01-01T00:00:00Z".into()
            )
        );
        sqlx::query("DELETE FROM invoices WHERE id='i'")
            .execute(&p)
            .await
            .unwrap();
        sqlx::query("DELETE FROM merchants")
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT reward_units FROM referral_earnings")
                .fetch_one(&p)
                .await
                .unwrap(),
            "50"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM direct_fee_receipts")
                .fetch_one(&p)
                .await
                .unwrap(),
            0
        );
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&p)
            .await
            .unwrap()
            .is_empty());
    }
}
