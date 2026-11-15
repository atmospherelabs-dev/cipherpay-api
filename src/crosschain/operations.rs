//! Merchant controls and read-only triage. Never recreate an ambiguous swap.
use super::*;

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("ALTER TABLE merchants ADD COLUMN crosschain_enabled INTEGER NOT NULL DEFAULT 1 CHECK(crosschain_enabled IN (0,1))").execute(pool).await?;
    Ok(())
}
pub async fn migrate_dismissals(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("CREATE TABLE crosschain_dismissals (
        invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
        prior_state TEXT NOT NULL, reason TEXT NOT NULL,
        dismissed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')))").execute(pool).await?;
    Ok(())
}
pub async fn enabled(pool: &SqlitePool, invoice: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar("SELECT m.crosschain_enabled FROM merchants m JOIN invoices i ON i.merchant_id=m.id WHERE i.id=?")
        .bind(invoice).fetch_optional(pool).await?.unwrap_or(false))
}
pub async fn issues(pool: &SqlitePool) -> anyhow::Result<Vec<Value>> {
    let rows: Vec<(String,String,String,String,i64)> = sqlx::query_as(
        "SELECT i.id,i.memo_code,a.state,
        CASE WHEN a.state IN ('CREATING','NEEDS_REVIEW') THEN 'setup_review'
             WHEN f.state NOT IN ('collected','direct','provider_handled') AND i.status='confirmed' AND f.failures>=3 THEN 'fee_verification'
             WHEN a.state IN ('FAILED','INCOMPLETE_DEPOSIT') THEN 'payment_review'
             ELSE 'settlement_delayed' END,
        COALESCE(f.failures,0)
        FROM crosschain_attempts a JOIN invoices i ON i.id=a.invoice_id
        LEFT JOIN crosschain_fees f ON f.invoice_id=i.id
        WHERE (a.state='NEEDS_REVIEW'
           OR (a.state='CREATING' AND i.created_at<strftime('%Y-%m-%dT%H:%M:%SZ','now','-5 minutes'))
           OR (f.state NOT IN ('collected','direct','provider_handled') AND i.status='confirmed' AND f.failures>=3)
           OR (a.state IN ('FAILED','INCOMPLETE_DEPOSIT') AND i.status NOT IN ('cancelled','refunded'))
           OR (a.state IN ('KNOWN_DEPOSIT_TX','PROCESSING','SUCCESS') AND i.status NOT IN ('confirmed','refunded','cancelled')
               AND i.expires_at<strftime('%Y-%m-%dT%H:%M:%SZ','now')))
        AND (i.status='confirmed' OR NOT EXISTS(SELECT 1 FROM crosschain_dismissals d WHERE d.invoice_id=i.id))
        ORDER BY i.created_at LIMIT 100")
        .fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(id,reference,state,reason,failures)|json!({"invoice_id":id,"reference":reference,"state":state,"reason":reason,"verification_failures":failures})).collect())
}
pub async fn admin_issues(req: actix_web::HttpRequest, pool: web::Data<SqlitePool>) -> HttpResponse {
    if !crate::api::admin::authenticate_admin(&req) { return HttpResponse::Unauthorized().finish(); }
    match issues(&pool).await {
        Ok(issues)=>HttpResponse::Ok().insert_header(("Cache-Control","no-store")).json(json!({"issues":issues})),
        Err(_)=>HttpResponse::ServiceUnavailable().json(json!({"error":"Payment monitoring unavailable"})),
    }
}
/// Retry only reconciliation of the existing deposit. No new quote or payment.
pub async fn retry(req: actix_web::HttpRequest,pool:web::Data<SqlitePool>,path:web::Path<String>) -> HttpResponse {
    if !crate::api::admin::authenticate_admin(&req) { return HttpResponse::Unauthorized().finish(); }
    let result=async {
        let mut tx=pool.begin().await?;
        let found:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM crosschain_attempts WHERE invoice_id=? AND payload IS NOT NULL)").bind(path.as_str()).fetch_one(&mut *tx).await?;
        ensure!(found,"No saved deposit to reconcile. Review setup with the provider; do not create another swap.");
        sqlx::query("UPDATE crosschain_fees SET next_check=0 WHERE invoice_id=? AND state NOT IN ('collected','direct','provider_handled')").bind(path.as_str()).execute(&mut *tx).await?;
        sqlx::query("UPDATE crosschain_attempts SET last_checked=0 WHERE invoice_id=?").bind(path.as_str()).execute(&mut *tx).await?;
        tx.commit().await?;
        anyhow::Ok(())
    }.await;
    match result {
        Ok(())=>HttpResponse::Ok().json(json!({"ok":true})),
        Err(e)=>HttpResponse::Conflict().json(json!({"error":e.to_string()})),
    }
}

#[derive(serde::Deserialize)]
pub struct DismissInput {
    reason: String,
}

/// Close a setup review that provably never produced a payable quote.
/// Only an attempt with no saved quote, no wallet send, an expired or
/// cancelled invoice and no received ZEC qualifies. Nothing is deleted:
/// the attempt and fee reservation stay as they are, and the dismissal
/// row records why. A later confirmation of the invoice re-surfaces it.
pub(crate) async fn dismiss_unquoted(pool: &SqlitePool, id: &str, reason: &str) -> anyhow::Result<()> {
    let reason = reason.trim();
    ensure!((3..=500).contains(&reason.chars().count()), "Give a reason of 3 to 500 characters");
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, bool, bool, String, i64, i64)> = sqlx::query_as(
        "SELECT a.state, a.payload IS NULL, a.send_token IS NULL AND a.sent_txid IS NULL, i.status, i.received_zatoshis,
                (SELECT COUNT(*) FROM invoice_payments p WHERE p.invoice_id=i.id)
         FROM crosschain_attempts a JOIN invoices i ON i.id=a.invoice_id WHERE a.invoice_id=?")
        .bind(id).fetch_optional(&mut *tx).await?;
    let (state, unquoted, unsent, status, received, payments) = row.context("No cross-chain attempt for this invoice")?;
    ensure!(matches!(state.as_str(), "NEEDS_REVIEW" | "CREATING"), "Only setup reviews can be dismissed");
    ensure!(unquoted && unsent, "A quote or wallet send exists; review it with the provider instead");
    ensure!(matches!(status.as_str(), "expired" | "cancelled"), "Invoice must be expired or cancelled");
    ensure!(received == 0 && payments == 0, "Invoice has received ZEC");
    let fee_ok: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM crosschain_fees WHERE invoice_id=? AND (state!='reserved' OR correlation_id IS NOT NULL))")
        .bind(id).fetch_one(&mut *tx).await?;
    ensure!(fee_ok, "Fee record is bound to a quote");
    let inserted = sqlx::query("INSERT OR IGNORE INTO crosschain_dismissals(invoice_id,prior_state,reason) VALUES (?,?,?)")
        .bind(id).bind(&state).bind(reason).execute(&mut *tx).await?;
    ensure!(inserted.rows_affected() == 1, "Already dismissed");
    tx.commit().await?;
    tracing::info!(invoice_id = %id, prior_state = %state, "Cross-chain setup review dismissed");
    Ok(())
}

pub async fn dismiss(req: actix_web::HttpRequest, pool: web::Data<SqlitePool>, path: web::Path<String>, input: web::Json<DismissInput>) -> HttpResponse {
    if !crate::api::admin::authenticate_admin(&req) { return HttpResponse::Unauthorized().finish(); }
    match dismiss_unquoted(&pool, path.as_str(), &input.reason).await {
        Ok(()) => HttpResponse::Ok().json(json!({"ok":true})),
        Err(e) => HttpResponse::Conflict().json(json!({"error":e.to_string()})),
    }
}

#[cfg(test)]
mod dismiss_tests {
    use super::*;
    use crate::repair_tests::{invoice, merchant, pool};

    async fn unquoted_review(p: &SqlitePool, id: &str) {
        invoice(p, id, "m").await;
        sqlx::query("UPDATE invoices SET status='expired' WHERE id=?").bind(id).execute(p).await.unwrap();
        sqlx::query("INSERT INTO crosschain_attempts(invoice_id,state) VALUES (?,'NEEDS_REVIEW')").bind(id).execute(p).await.unwrap();
        sqlx::query("INSERT INTO crosschain_fees(invoice_id,asset_id,recipient,total_bps,cipherpay_bps,failures) VALUES (?,'usdc','cipherscan.near',145,125,564)")
            .bind(id).execute(p).await.unwrap();
    }
    async fn listed(p: &SqlitePool, id: &str) -> bool {
        issues(p).await.unwrap().iter().any(|i| i["invoice_id"] == id)
    }

    #[tokio::test]
    async fn dismisses_only_unquoted_unpaid_expired_setup_and_keeps_records() {
        let p = pool().await;
        merchant(&p, "m").await;
        unquoted_review(&p, "a").await;
        assert!(listed(&p, "a").await);
        assert!(dismiss_unquoted(&p, "a", "  ").await.unwrap_err().to_string().contains("reason"));
        dismiss_unquoted(&p, "a", "Provider never returned a quote; no deposit address issued").await.unwrap();
        assert!(!listed(&p, "a").await);
        assert!(dismiss_unquoted(&p, "a", "again please").await.unwrap_err().to_string().contains("Already dismissed"));
        // Attempt and fee reservation are untouched; the dismissal is recorded.
        let attempt: (String, Option<String>) = sqlx::query_as("SELECT state,payload FROM crosschain_attempts WHERE invoice_id='a'").fetch_one(&p).await.unwrap();
        assert_eq!(attempt, ("NEEDS_REVIEW".to_string(), None));
        let fee: (String, i64) = sqlx::query_as("SELECT state,failures FROM crosschain_fees WHERE invoice_id='a'").fetch_one(&p).await.unwrap();
        assert_eq!(fee, ("reserved".to_string(), 564));
        let d: (String, String) = sqlx::query_as("SELECT prior_state,reason FROM crosschain_dismissals WHERE invoice_id='a'").fetch_one(&p).await.unwrap();
        assert_eq!(d.0, "NEEDS_REVIEW");
        // A later confirmation re-surfaces it for review.
        sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='a'").execute(&p).await.unwrap();
        assert!(listed(&p, "a").await);
    }

    #[tokio::test]
    async fn refuses_quoted_sent_unexpired_or_paid_attempts() {
        let p = pool().await;
        merchant(&p, "m").await;
        let reason = "test dismissal";
        unquoted_review(&p, "quoted").await;
        sqlx::query("UPDATE crosschain_attempts SET payload='enc' WHERE invoice_id='quoted'").execute(&p).await.unwrap();
        assert!(dismiss_unquoted(&p, "quoted", reason).await.unwrap_err().to_string().contains("quote or wallet send"));
        unquoted_review(&p, "sent").await;
        sqlx::query("UPDATE crosschain_attempts SET send_token='t' WHERE invoice_id='sent'").execute(&p).await.unwrap();
        assert!(dismiss_unquoted(&p, "sent", reason).await.is_err());
        unquoted_review(&p, "pending").await;
        sqlx::query("UPDATE invoices SET status='pending' WHERE id='pending'").execute(&p).await.unwrap();
        assert!(dismiss_unquoted(&p, "pending", reason).await.unwrap_err().to_string().contains("expired or cancelled"));
        unquoted_review(&p, "paid").await;
        sqlx::query("INSERT INTO invoice_payments(invoice_id,txid,zatoshis) VALUES ('paid',?,10)").bind("ab".repeat(32)).execute(&p).await.unwrap();
        assert!(dismiss_unquoted(&p, "paid", reason).await.unwrap_err().to_string().contains("received ZEC"));
        unquoted_review(&p, "bound").await;
        sqlx::query("UPDATE crosschain_fees SET correlation_id='c' WHERE invoice_id='bound'").execute(&p).await.unwrap();
        assert!(dismiss_unquoted(&p, "bound", reason).await.unwrap_err().to_string().contains("Fee record"));
        unquoted_review(&p, "failed").await;
        sqlx::query("UPDATE crosschain_attempts SET state='FAILED' WHERE invoice_id='failed'").execute(&p).await.unwrap();
        assert!(dismiss_unquoted(&p, "failed", reason).await.unwrap_err().to_string().contains("setup reviews"));
        assert!(dismiss_unquoted(&p, "missing", reason).await.is_err());
        for id in ["quoted", "sent", "pending", "paid", "bound", "failed"] {
            assert!(listed(&p, id).await || id == "pending", "{id} should stay flagged");
        }
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM crosschain_dismissals").fetch_one(&p).await.unwrap(), 0);
    }
}
