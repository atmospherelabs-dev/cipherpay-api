//! Merchant controls and read-only triage. Never recreate an ambiguous swap.
use super::*;

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("ALTER TABLE merchants ADD COLUMN crosschain_enabled INTEGER NOT NULL DEFAULT 1 CHECK(crosschain_enabled IN (0,1))").execute(pool).await?;
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
