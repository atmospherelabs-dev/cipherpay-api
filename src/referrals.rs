//! Referral earnings are a share of collected revenue, in the asset received.
//! No automatic payout, FX conversion, or credit against unrelated assets.
use actix_web::{web, HttpRequest, HttpResponse};
use anyhow::{ensure, Context};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
pub mod guardrails;

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::raw_sql("CREATE TABLE referral_codes (
      merchant_id TEXT PRIMARY KEY REFERENCES merchants(id) ON DELETE CASCADE,
      code TEXT NOT NULL UNIQUE);
      CREATE TABLE merchant_referrals (
        merchant_id TEXT PRIMARY KEY REFERENCES merchants(id),
        referrer_id TEXT NOT NULL REFERENCES merchants(id),
        applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
        discount_until TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now','+3 months')),
        earning_until TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now','+12 months')),
        CHECK (merchant_id != referrer_id));
      CREATE TABLE invoice_fee_policy (
        invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
        service_bps INTEGER NOT NULL CHECK(service_bps BETWEEN 0 AND 1000),
        referrer_id TEXT REFERENCES merchants(id));
      CREATE TRIGGER invoice_fee_policy_snapshot AFTER INSERT ON invoices BEGIN
        INSERT INTO invoice_fee_policy(invoice_id,service_bps,referrer_id)
        SELECT NEW.id,
          MIN(1000,MAX(0,CAST(ROUND(COALESCE(m.fee_rate,0.01)*10000) AS INTEGER))),
          CASE WHEN r.earning_until>NEW.created_at THEN r.referrer_id END
        FROM merchants m LEFT JOIN merchant_referrals r ON r.merchant_id=m.id WHERE m.id=NEW.merchant_id;
        UPDATE invoice_fee_policy SET service_bps=MIN(service_bps,50)
          WHERE invoice_id=NEW.id AND EXISTS(SELECT 1 FROM merchant_referrals WHERE merchant_id=NEW.merchant_id AND discount_until>NEW.created_at);
      END;
      CREATE TABLE referral_earnings (
        receipt_id TEXT PRIMARY KEY,
        invoice_id TEXT NOT NULL REFERENCES invoices(id),
        referrer_id TEXT NOT NULL REFERENCES merchants(id),
        asset_id TEXT NOT NULL, collected_units TEXT NOT NULL, reward_units TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
      ALTER TABLE fee_ledger ADD COLUMN collected_zatoshis INTEGER NOT NULL DEFAULT 0;
      UPDATE fee_ledger SET collected_zatoshis=fee_amount_zatoshis WHERE auto_collected=1;
      CREATE TABLE direct_fee_checks (
        invoice_id TEXT NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
        txid TEXT NOT NULL, next_check INTEGER NOT NULL, PRIMARY KEY(invoice_id,txid));
      CREATE TABLE direct_fee_receipts (
        invoice_id TEXT NOT NULL REFERENCES invoices(id), txid TEXT NOT NULL,
        units INTEGER NOT NULL CHECK(units>0), PRIMARY KEY(invoice_id,txid));")
        .execute(&mut *tx).await?;
    // Existing quotes retain their stored fee policy; never retroactively award.
    sqlx::query(
        "INSERT INTO invoice_fee_policy(invoice_id,service_bps) SELECT id,100 FROM invoices",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn service_bps(pool: &SqlitePool, invoice: &str) -> anyhow::Result<u64> {
    let bps: i64 =
        sqlx::query_scalar("SELECT service_bps FROM invoice_fee_policy WHERE invoice_id=?")
            .bind(invoice)
            .fetch_one(pool)
            .await?;
    Ok(bps as u64)
}

pub async fn accrue(
    pool: &SqlitePool,
    receipt: &str,
    invoice: &str,
    asset: &str,
    units: u128,
) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    accrue_on(&mut conn, receipt, invoice, asset, units).await
}

async fn accrue_on(
    conn: &mut sqlx::SqliteConnection,
    receipt: &str,
    invoice: &str,
    asset: &str,
    units: u128,
) -> anyhow::Result<()> {
    ensure!(units > 0, "Empty receipt");
    let referrer: Option<String> = sqlx::query_scalar(
        "SELECT p.referrer_id FROM invoice_fee_policy p JOIN invoices i ON i.id=p.invoice_id
        WHERE p.invoice_id=? AND i.status='confirmed'",
    )
    .bind(invoice)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    let Some(referrer) = referrer else {
        return Ok(());
    };
    // Round down in smallest asset units. Replays never mint another reward.
    sqlx::query("INSERT INTO referral_earnings(receipt_id,invoice_id,referrer_id,asset_id,collected_units,reward_units) VALUES (?,?,?,?,?,?) ON CONFLICT(receipt_id) DO NOTHING")
        .bind(receipt).bind(invoice).bind(referrer).bind(asset).bind(units.to_string()).bind((units/2).to_string()).execute(&mut *conn).await?;
    let stored: (String, String, String) = sqlx::query_as(
        "SELECT invoice_id,asset_id,collected_units FROM referral_earnings WHERE receipt_id=?",
    )
    .bind(receipt)
    .fetch_one(&mut *conn)
    .await?;
    ensure!(
        stored == (invoice.to_string(), asset.to_string(), units.to_string()),
        "Receipt attribution changed"
    );
    Ok(())
}

/// Keep earned commissions even when their referred merchant closes an account.
/// Called under the same write transaction as deletion so receipt reconciliation
/// cannot race deletion and lose a referrer's earnings.
pub(crate) async fn before_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    merchant: &str,
) -> anyhow::Result<()> {
    guardrails::check_deletion(tx, merchant).await?;
    guardrails::remember(tx, merchant).await?;
    let swaps: Vec<(String,String,String)> = sqlx::query_as("SELECT f.invoice_id,f.asset_id,f.received_units FROM crosschain_fees f JOIN invoices i ON i.id=f.invoice_id WHERE i.merchant_id=? AND i.status='confirmed' AND f.state='collected'")
        .bind(merchant).fetch_all(&mut **tx).await?;
    for (id, asset, units) in swaps {
        accrue_on(
            &mut **tx,
            &format!("swap:{id}"),
            &id,
            &asset,
            units.parse()?,
        )
        .await?;
    }
    let direct: Vec<(String,String,i64)> = sqlx::query_as("SELECT r.invoice_id,r.txid,r.units FROM direct_fee_receipts r JOIN invoices i ON i.id=r.invoice_id WHERE i.merchant_id=? AND i.status='confirmed'")
        .bind(merchant).fetch_all(&mut **tx).await?;
    for (id, txid, units) in direct {
        accrue_on(
            &mut **tx,
            &format!("zec:{id}:{txid}"),
            &id,
            "zec",
            units as u128,
        )
        .await?;
    }
    Ok(())
}

/// Replay actual receipts after confirmation, including after process restarts.
pub async fn reconcile(pool: &SqlitePool) -> anyhow::Result<()> {
    let cross: Vec<(String,String,String)> = sqlx::query_as("SELECT f.invoice_id,f.asset_id,f.received_units FROM crosschain_fees f JOIN invoices i ON i.id=f.invoice_id JOIN invoice_fee_policy p ON p.invoice_id=i.id
      WHERE f.state='collected' AND i.status='confirmed' AND p.referrer_id IS NOT NULL
      AND NOT EXISTS(SELECT 1 FROM referral_earnings e WHERE e.receipt_id='swap:'||i.id) LIMIT 100")
        .fetch_all(pool).await?;
    for (id, asset, units) in cross {
        accrue(pool, &format!("swap:{id}"), &id, &asset, units.parse()?).await?;
    }
    let direct: Vec<(String,String,i64)> = sqlx::query_as("SELECT r.invoice_id,r.txid,r.units FROM direct_fee_receipts r JOIN invoices i ON i.id=r.invoice_id JOIN invoice_fee_policy p ON p.invoice_id=i.id
      WHERE i.status='confirmed' AND p.referrer_id IS NOT NULL
      AND NOT EXISTS(SELECT 1 FROM referral_earnings e WHERE e.receipt_id='zec:'||r.invoice_id||':'||r.txid) LIMIT 100")
        .fetch_all(pool).await?;
    for (id, txid, units) in direct {
        accrue(pool, &format!("zec:{id}:{txid}"), &id, "zec", units as u128).await?;
    }
    Ok(())
}

pub async fn summary(pool: &SqlitePool, merchant: &str, key: &str) -> anyhow::Result<Value> {
    sqlx::query("INSERT INTO referral_codes(merchant_id,code) VALUES (?,?) ON CONFLICT(merchant_id) DO NOTHING")
        .bind(merchant).bind(format!("CPREF-{}",hex::encode(rand::random::<[u8;8]>()).to_uppercase())).execute(pool).await?;
    let code: String = sqlx::query_scalar("SELECT code FROM referral_codes WHERE merchant_id=?")
        .bind(merchant)
        .fetch_one(pool)
        .await?;
    let applied: Option<(String, String)> = sqlx::query_as(
        "SELECT discount_until,earning_until FROM merchant_referrals WHERE merchant_id=?",
    )
    .bind(merchant)
    .fetch_optional(pool)
    .await?;
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM merchant_referrals WHERE referrer_id=?")
            .bind(merchant)
            .fetch_one(pool)
            .await?;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT asset_id,reward_units FROM referral_earnings WHERE referrer_id=?")
            .bind(merchant)
            .fetch_all(pool)
            .await?;
    let mut totals = std::collections::BTreeMap::<String, u128>::new();
    for (asset, units) in rows {
        let v = totals.entry(asset).or_default();
        *v = v
            .checked_add(units.parse()?)
            .context("Referral balance overflow")?;
    }
    let mut balances = Vec::new();
    for (asset, units) in totals {
        let payload: Option<String>=sqlx::query_scalar("SELECT a.payload FROM crosschain_attempts a JOIN crosschain_fees f ON f.invoice_id=a.invoice_id JOIN referral_earnings e ON e.invoice_id=a.invoice_id WHERE f.asset_id=? AND e.referrer_id=? LIMIT 1")
            .bind(&asset).bind(merchant).fetch_optional(pool).await?.flatten();
        let metadata = payload
            .and_then(|v| crate::crypto::decrypt(&v, key).ok())
            .and_then(|v| serde_json::from_str::<Value>(&v).ok())
            .map(|v| v["asset"].clone());
        balances.push(json!({"asset_id":asset,"units":units.to_string(),"asset":metadata}));
    }
    Ok(
        json!({"code":code,"referred_merchants":count,"applied":applied.is_some(),"discount_until":applied.map(|v|v.0),
        "reward_bps":5000,"payouts_enabled":false,
        "balances":balances}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Apply {
    code: String,
}
pub async fn apply(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    input: web::Json<Apply>,
) -> HttpResponse {
    if !config.referrals_enabled {
        return HttpResponse::NotFound().json(json!({"error":"Referral program is not available"}));
    }
    let merchant = match crate::api::auth::require_full_session(&req, &pool).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    match apply_code(&pool, &merchant.id, input.code.trim()).await {
        Ok(()) => HttpResponse::Ok().json(json!({"ok":true})),
        Err(e) => HttpResponse::BadRequest().json(json!({"error":e.to_string()})),
    }
}
async fn apply_code(pool: &SqlitePool, merchant: &str, code: &str) -> anyhow::Result<()> {
    ensure!(code.len() <= 40, "Invalid referral code");
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let referrer: String =
        sqlx::query_scalar("SELECT merchant_id FROM referral_codes WHERE code=?")
            .bind(code.to_ascii_uppercase())
            .fetch_optional(&mut *tx)
            .await?
            .context("Referral code not found")?;
    ensure!(merchant != referrer, "You cannot refer yourself");
    // Also reject shared merchant receivers; wallet/account uniqueness alone is
    // not Sybil resistance. Economics remain bounded by revenue actually earned.
    let shared: bool=sqlx::query_scalar("SELECT a.payment_address=b.payment_address FROM merchants a,merchants b WHERE a.id=? AND b.id=?")
        .bind(merchant).bind(&referrer).fetch_one(&mut *tx).await?;
    ensure!(!shared, "A referral must be from a different merchant");
    guardrails::check(&mut tx, merchant, &referrer).await?;
    let inserted=sqlx::query("INSERT INTO merchant_referrals(merchant_id,referrer_id) VALUES (?,?) ON CONFLICT(merchant_id) DO NOTHING")
        .bind(merchant).bind(referrer).execute(&mut *tx).await?.rows_affected();
    ensure!(inserted == 1, "A referral has already been applied");
    guardrails::remember(&mut tx, merchant).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repair_tests::{invoice, merchant, pool};
    async fn fixture() -> SqlitePool {
        let p = pool().await;
        for m in ["referrer", "buyer", "other"] {
            merchant(&p, m).await;
            sqlx::query("UPDATE merchants SET payment_address=? WHERE id=?")
                .bind(format!("address-{m}"))
                .bind(m)
                .execute(&p)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO referral_codes VALUES ('referrer','CPREF-TEST')")
            .execute(&p)
            .await
            .unwrap();
        p
    }
    #[tokio::test]
    async fn discount_and_earnings_are_forward_only_and_snapshotted() {
        let p = fixture().await;
        invoice(&p, "before", "buyer").await;
        apply_code(&p, "buyer", "CPREF-TEST").await.unwrap();
        invoice(&p, "during", "buyer").await;
        assert_eq!(service_bps(&p, "before").await.unwrap(), 100);
        assert_eq!(service_bps(&p, "during").await.unwrap(), 50);
        sqlx::query("UPDATE merchant_referrals SET discount_until='2000-01-01T00:00:00Z'")
            .execute(&p)
            .await
            .unwrap();
        invoice(&p, "after-discount", "buyer").await;
        assert_eq!(service_bps(&p, "after-discount").await.unwrap(), 100);
        assert_eq!(service_bps(&p, "during").await.unwrap(), 50);
        sqlx::query("UPDATE merchant_referrals SET earning_until='2000-01-01T00:00:00Z'")
            .execute(&p)
            .await
            .unwrap();
        invoice(&p, "after-program", "buyer").await;
        let referrer: Option<String> = sqlx::query_scalar(
            "SELECT referrer_id FROM invoice_fee_policy WHERE invoice_id='after-program'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert!(referrer.is_none());
    }
    #[tokio::test]
    async fn receipt_replays_cannot_mint_rewards_or_mix_assets() {
        let p = fixture().await;
        apply_code(&p, "buyer", "CPREF-TEST").await.unwrap();
        invoice(&p, "one", "buyer").await;
        accrue(&p, "receipt", "one", "usdc-base", 69581)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM referral_earnings")
                .fetch_one(&p)
                .await
                .unwrap(),
            0
        );
        sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='one'")
            .execute(&p)
            .await
            .unwrap();
        for _ in 0..3 {
            accrue(&p, "receipt", "one", "usdc-base", 69581)
                .await
                .unwrap();
        }
        assert!(accrue(&p, "receipt", "one", "usdc-sol", 69581)
            .await
            .is_err());
        accrue(&p, "zec-receipt", "one", "zec", 101).await.unwrap();
        let balances = summary(&p, "referrer", "").await.unwrap();
        assert_eq!(balances["balances"][0]["units"], "34790");
        assert_eq!(balances["balances"][1]["units"], "50");
        assert!(summary(&p, "other", "").await.unwrap()["balances"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[tokio::test]
    async fn apply_is_once_only_and_rejects_self_and_shared_receiver() {
        let p = fixture().await;
        assert!(apply_code(&p, "referrer", "CPREF-TEST").await.is_err());
        sqlx::query("UPDATE merchants SET payment_address='address-referrer' WHERE id='other'")
            .execute(&p)
            .await
            .unwrap();
        assert!(apply_code(&p, "other", "CPREF-TEST").await.is_err());
        let (a, b) = tokio::join!(
            apply_code(&p, "buyer", "CPREF-TEST"),
            apply_code(&p, "buyer", "CPREF-TEST")
        );
        assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    }

    #[tokio::test]
    async fn deleting_and_recreating_a_wallet_does_not_restart_referrals_or_erase_rewards() {
        let p = fixture().await;
        apply_code(&p, "buyer", "CPREF-TEST").await.unwrap();
        invoice(&p, "sale", "buyer").await;
        sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='sale'")
            .execute(&p)
            .await
            .unwrap();
        // The receipt arrives just before deletion, before the normal reward worker.
        sqlx::query("INSERT INTO direct_fee_receipts VALUES ('sale','fee-tx',1000)")
            .execute(&p)
            .await
            .unwrap();
        sqlx::query("UPDATE merchant_referrals SET discount_until='2000-01-01T00:00:00Z',earning_until='2001-01-01T00:00:00Z' WHERE merchant_id='buyer'").execute(&p).await.unwrap();
        crate::merchants::delete_merchant(&p, "buyer")
            .await
            .unwrap();
        assert_eq!(
            summary(&p, "referrer", "").await.unwrap()["balances"][0]["units"],
            "500"
        );
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&p)
            .await
            .unwrap();
        assert!(violations.is_empty());
        merchant(&p, "replacement").await;
        sqlx::query("UPDATE merchants SET payment_address='address-buyer' WHERE id='replacement'")
            .execute(&p)
            .await
            .unwrap();
        assert!(apply_code(&p, "replacement", "CPREF-TEST")
            .await
            .unwrap_err()
            .to_string()
            .contains("already used"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM merchant_referrals WHERE merchant_id='replacement'"
            )
            .fetch_one(&p)
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM referral_wallet_history")
                .fetch_one(&p)
                .await
                .unwrap(),
            1
        );
        crate::merchants::delete_merchant(&p, "referrer")
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM referral_earnings")
                .fetch_one(&p)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn referral_loops_and_concurrent_wallet_aliases_are_rejected() {
        let p = fixture().await;
        apply_code(&p, "buyer", "CPREF-TEST").await.unwrap();
        let buyer_code = summary(&p, "buyer", "").await.unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        apply_code(&p, "other", &buyer_code).await.unwrap();
        let other_code = summary(&p, "other", "").await.unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(apply_code(&p, "referrer", &buyer_code)
            .await
            .unwrap_err()
            .to_string()
            .contains("loop"));
        assert!(apply_code(&p, "referrer", &other_code)
            .await
            .unwrap_err()
            .to_string()
            .contains("loop"));
        for id in ["alias-a", "alias-b"] {
            merchant(&p, id).await;
        }
        let (a, b) = tokio::join!(
            apply_code(&p, "alias-a", "CPREF-TEST"),
            apply_code(&p, "alias-b", "CPREF-TEST")
        );
        assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    }

    #[tokio::test]
    async fn account_closure_waits_for_fee_verification_and_actual_collection() {
        let p = fixture().await;
        apply_code(&p, "buyer", "CPREF-TEST").await.unwrap();
        invoice(&p, "sale", "buyer").await;
        sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='sale'")
            .execute(&p)
            .await
            .unwrap();
        sqlx::query("INSERT INTO deferred_payment_fees(invoice_id,merchant_id,amount_zec,rate) VALUES ('sale','buyer',0.001,0.01)").execute(&p).await.unwrap();
        assert!(crate::merchants::delete_merchant(&p, "buyer")
            .await
            .unwrap_err()
            .downcast_ref::<guardrails::PendingAccountFees>()
            .is_some());
        sqlx::query("UPDATE deferred_payment_fees SET state='direct' WHERE invoice_id='sale'")
            .execute(&p)
            .await
            .unwrap();
        crate::billing::create_fee_entry(&p, "sale", "buyer", 0.001, 0.01)
            .await
            .unwrap();
        assert!(crate::merchants::delete_merchant(&p, "buyer")
            .await
            .unwrap_err()
            .downcast_ref::<guardrails::PendingAccountFees>()
            .is_some());
        crate::billing::record_direct_receipt(&p, "sale", "fee-tx", 100000)
            .await
            .unwrap();
        crate::merchants::delete_merchant(&p, "buyer")
            .await
            .unwrap();
        assert_eq!(
            summary(&p, "referrer", "").await.unwrap()["balances"][0]["units"],
            "50000"
        );
    }
}

#[cfg(test)]
mod settlement_tests {
    use super::*;
    #[tokio::test]
    async fn postpaid_rewards_wait_for_real_settlement_and_cannot_replay() {
        let p = crate::repair_tests::pool().await;
        for id in ["merchant", "referrer"] {
            crate::repair_tests::merchant(&p, id).await;
        }
        sqlx::query("INSERT INTO merchant_referrals(merchant_id,referrer_id) VALUES ('merchant','referrer')").execute(&p).await.unwrap();
        crate::repair_tests::invoice(&p, "sale", "merchant").await;
        sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='sale'")
            .execute(&p)
            .await
            .unwrap();
        crate::repair_tests::invoice(&p, "bill", "merchant").await;
        sqlx::query("INSERT INTO billing_cycles(id,merchant_id,period_start,period_end,status) VALUES ('cycle','merchant','2026-01-01','2026-02-01','open')").execute(&p).await.unwrap();
        crate::billing::create_fee_entry(&p, "sale", "merchant", 0.001, 0.01)
            .await
            .unwrap();
        sqlx::query("UPDATE billing_cycles SET status='invoiced',settlement_invoice_id='bill' WHERE id='cycle'").execute(&p).await.unwrap();
        reconcile(&p).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM referral_earnings")
                .fetch_one(&p)
                .await
                .unwrap(),
            0
        );
        sqlx::query("UPDATE invoices SET status='confirmed',price_zatoshis=100000,received_zatoshis=100000 WHERE id='bill'").execute(&p).await.unwrap();
        let config = crate::config::Config::from_env().unwrap();
        for _ in 0..2 {
            crate::billing::check_settlement_payments(&p, &config)
                .await
                .unwrap();
        }
        let reward: String = sqlx::query_scalar(
            "SELECT reward_units FROM referral_earnings WHERE invoice_id='sale'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(reward, "50000");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM referral_earnings")
                .fetch_one(&p)
                .await
                .unwrap(),
            1
        );
    }
}
