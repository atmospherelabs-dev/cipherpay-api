use sqlx::SqlitePool;

pub async fn pool() -> SqlitePool {
    crate::db::create_pool("sqlite::memory:").await.unwrap()
}
pub async fn merchant(pool: &SqlitePool, id: &str) {
    sqlx::query("INSERT INTO merchants (id, api_key_hash, ufvk, payment_address, webhook_url) VALUES (?, ?, ?, 'test-address', 'https://example.com/webhook')")
        .bind(id).bind(format!("hash-{id}")).bind(format!("key-{id}")).execute(pool).await.unwrap();
}
pub async fn invoice(pool: &SqlitePool, id: &str, merchant: &str) {
    sqlx::query("INSERT INTO invoices (id, merchant_id, memo_code, price_eur, price_zec, zec_rate_at_creation, price_zatoshis, expires_at) VALUES (?, ?, ?, 1, 0.00001, 100, 1000, '2099-01-01T00:00:00Z')")
        .bind(id).bind(merchant).bind(format!("CP-{id}")).execute(pool).await.unwrap();
}
#[tokio::test]
async fn session_spending_is_tenant_price_and_request_bound() {
    let pool = pool().await;
    merchant(&pool, "a").await;
    merchant(&pool, "b").await;
    let s = crate::sessions::create_session(&pool, "a", "deposit", 10_000, None)
        .await
        .unwrap();
    assert!(
        crate::sessions::charge(&pool, "b", &s.bearer_token, 5000, "/expensive", "r1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::sessions::charge(&pool, "a", &s.bearer_token, 20_000, "/expensive", "r1")
            .await
            .unwrap()
            .is_none()
    );
    let paid = crate::sessions::charge(&pool, "a", &s.bearer_token, 6000, "/expensive", "r1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paid.balance_remaining, 4000);
    assert!(
        crate::sessions::charge(&pool, "a", &s.bearer_token, 1000, "/cheap", "r1")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        crate::sessions::get_session(&pool, &s.id)
            .await
            .unwrap()
            .unwrap()
            .balance_remaining,
        4000
    );
    let (a, b) = tokio::join!(
        crate::sessions::charge(&pool, "a", &s.bearer_token, 3000, "/x", "r2"),
        crate::sessions::charge(&pool, "a", &s.bearer_token, 3000, "/x", "r3")
    );
    assert_eq!(a.unwrap().is_some() as u8 + b.unwrap().is_some() as u8, 1);
}
#[tokio::test]
async fn consumed_deposit_survives_session_and_account_deletion() {
    let pool = pool().await;
    merchant(&pool, "a").await;
    let s = crate::sessions::create_session(&pool, "a", "ABCDEF", 10_000, None)
        .await
        .unwrap();
    sqlx::query("DELETE FROM agent_sessions WHERE id = ?")
        .bind(&s.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        crate::sessions::create_session(&pool, "a", "abcdef", 10_000, None)
            .await
            .is_err()
    );
    crate::merchants::delete_merchant(&pool, "a").await.unwrap();
    assert!(crate::sessions::txid_already_used(&pool, "ABCDEF").await);
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fk, 1);
}
#[tokio::test]
async fn payment_ledger_and_outbox_are_idempotent_and_atomic() {
    let pool = pool().await;
    merchant(&pool, "a").await;
    invoice(&pool, "one", "a").await;
    assert_eq!(
        crate::invoices::record_payment(&pool, "one", "part1", 400)
            .await
            .unwrap(),
        400
    );
    crate::invoices::mark_underpaid(&pool, "one", 400, "part1")
        .await
        .unwrap();
    assert_eq!(
        crate::invoices::record_payment(&pool, "one", "part1", 400)
            .await
            .unwrap(),
        400
    );
    assert_eq!(
        crate::invoices::record_payment(&pool, "one", "part2", 600)
            .await
            .unwrap(),
        1000
    );
    assert!(crate::invoices::mark_detected(&pool, "one", "part2", 1000)
        .await
        .unwrap());
    assert!(!crate::invoices::mark_detected(&pool, "one", "part2", 1000)
        .await
        .unwrap());
    let contributions: Vec<i64> =
        sqlx::query_scalar("SELECT zatoshis FROM invoice_payments ORDER BY zatoshis")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(contributions, vec![400, 600]);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='one'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE event_type='confirmed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    assert!(crate::invoices::mark_confirmed(&pool, "one", None, None)
        .await
        .unwrap());
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_deliveries WHERE event_type='confirmed' AND attempts=0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}
#[tokio::test]
async fn account_deletion_rolls_back_on_failure() {
    let pool = pool().await;
    merchant(&pool, "a").await;
    invoice(&pool, "one", "a").await;
    crate::invoices::record_payment(&pool, "one", "tx", 1000)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER deny_delete BEFORE DELETE ON merchants BEGIN SELECT RAISE(ABORT,'injected failure'); END").execute(&pool).await.unwrap();
    assert!(crate::merchants::delete_merchant(&pool, "a").await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM invoice_payments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    sqlx::query("DROP TRIGGER deny_delete")
        .execute(&pool)
        .await
        .unwrap();
    crate::merchants::delete_merchant(&pool, "a").await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM invoices")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn abandoned_attendee_details_are_purged() {
    let pool = pool().await;
    merchant(&pool, "a").await;
    invoice(&pool, "one", "a").await;
    sqlx::query("UPDATE invoices SET status='expired', created_at='2020-01-01T00:00:00Z', attendee_name='encrypted',attendee_email='encrypted' WHERE id='one'").execute(&pool).await.unwrap();
    crate::db::run_data_purge(&pool, 30).await.unwrap();
    let pii: Option<String> =
        sqlx::query_scalar("SELECT attendee_email FROM invoices WHERE id='one'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(pii.is_none());
}

#[tokio::test]
async fn prepared_session_consumption_rolls_back_and_cannot_be_reused() {
    let pool = pool().await;
    merchant(&pool, "a").await;
    sqlx::query("INSERT INTO session_requests (id,merchant_id,deposit_address,diversifier_index,status,expires_at) VALUES ('request','a','address',1,'pending','2099-01-01T00:00:00Z')").execute(&pool).await.unwrap();
    assert!(
        crate::sessions::create_prepared_session(&pool, "a", "request", "tx", 1, None)
            .await
            .is_err()
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM session_requests WHERE id='request'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    assert!(!crate::sessions::txid_already_used(&pool, "tx").await);
    crate::sessions::create_prepared_session(&pool, "a", "request", "tx", 10000, None)
        .await
        .unwrap();
    assert!(
        crate::sessions::create_prepared_session(&pool, "a", "request", "other", 10000, None)
            .await
            .is_err()
    );
    assert!(!crate::sessions::txid_already_used(&pool, "other").await);
}
