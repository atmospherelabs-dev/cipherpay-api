//! Checkout mutation capabilities are separate from public invoice identifiers.
use super::*;
use actix_web::HttpRequest;

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::raw_sql("CREATE TABLE checkout_access (
        invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
        token TEXT NOT NULL UNIQUE);
        INSERT INTO checkout_access SELECT id,lower(hex(randomblob(32))) FROM invoices;
        CREATE TRIGGER invoice_checkout_access AFTER INSERT ON invoices BEGIN
          INSERT INTO checkout_access VALUES (NEW.id,lower(hex(randomblob(32)))); END;
        CREATE TABLE deferred_payment_fees (
          invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
          merchant_id TEXT NOT NULL REFERENCES merchants(id), amount_zec REAL NOT NULL,
          rate REAL NOT NULL, state TEXT NOT NULL DEFAULT 'verifying');
        ALTER TABLE crosschain_attempts ADD COLUMN price_expires_at TEXT;
        UPDATE crosschain_attempts SET price_expires_at=(SELECT MIN(expires_at,strftime('%Y-%m-%dT%H:%M:%SZ','now')) FROM invoices WHERE id=invoice_id);
        CREATE TABLE crosschain_deliveries (
          invoice_id TEXT NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
          txid TEXT NOT NULL, PRIMARY KEY(invoice_id,txid));")
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn token(pool: &SqlitePool, id: &str) -> anyhow::Result<String> {
    Ok(
        sqlx::query_scalar("SELECT token FROM checkout_access WHERE invoice_id=?")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn authorized(req: &HttpRequest, pool: &SqlitePool, id: &str) -> bool {
    let Some(supplied) = req
        .headers()
        .get("X-Checkout-Token")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    if supplied.len() != 64 {
        return false;
    }
    let Ok(expected) = token(pool, id).await else {
        return false;
    };
    // Fixed-length comparison; do not log this capability or put it in query strings.
    expected
        .bytes()
        .zip(supplied.bytes())
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

pub async fn access(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> HttpResponse {
    let merchant = match crate::api::auth::require_full_or_session(&req, &pool).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let owned = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM invoices WHERE id=? AND merchant_id=?)",
    )
    .bind(path.as_str())
    .bind(&merchant.id)
    .fetch_one(pool.get_ref())
    .await
    .unwrap_or(false);
    if !owned {
        return HttpResponse::NotFound().finish();
    }
    match token(&pool, &path).await {
        Ok(token) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(json!({"checkout_token":token})),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// During recovery the scanner may accept only the provider's verified delivery,
/// never an unrelated direct transfer at an obsolete fiat exchange rate.
pub async fn accepts_payment(pool: &SqlitePool, id: &str, txid: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM crosschain_attempts WHERE invoice_id=? AND price_expires_at<=strftime('%Y-%m-%dT%H:%M:%SZ','now'))
      OR EXISTS(SELECT 1 FROM crosschain_deliveries WHERE invoice_id=? AND lower(txid)=lower(?))
      OR EXISTS(SELECT 1 FROM invoice_payments WHERE invoice_id=? AND txid=?)")
        .bind(id).bind(id).bind(txid).bind(id).bind(txid).fetch_one(pool).await?)
}
