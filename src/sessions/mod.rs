use anyhow::Result;
use sqlx::SqlitePool;

pub struct Session {
    pub id: String,
    pub merchant_id: String,
    pub deposit_txid: String,
    pub bearer_token: String,
    pub balance_zatoshis: i64,
    pub balance_remaining: i64,
    pub cost_per_request: i64,
    pub requests_made: i64,
    pub refund_address: Option<String>,
    pub status: String,
    pub expires_at: String,
    pub created_at: String,
    pub closed_at: Option<String>,
}

type SessionRow = (
    String,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
);

pub struct SessionSummary {
    pub session_id: String,
    pub requests_made: i64,
    pub balance_used: i64,
    pub balance_remaining: i64,
    pub status: String,
    pub refund_address: Option<String>,
}

const DEFAULT_COST_PER_REQUEST: i64 = 1_000; // 0.00001 ZEC
const SESSION_EXPIRY_HOURS: i64 = 24;

pub async fn create_session(
    pool: &SqlitePool,
    merchant_id: &str,
    deposit_txid: &str,
    balance_zatoshis: i64,
    refund_address: Option<&str>,
) -> Result<Session> {
    create_session_with_cost(
        pool,
        merchant_id,
        deposit_txid,
        balance_zatoshis,
        refund_address,
        None,
    )
    .await
}

pub async fn create_session_with_cost(
    pool: &SqlitePool,
    merchant_id: &str,
    deposit_txid: &str,
    balance_zatoshis: i64,
    refund_address: Option<&str>,
    cost_per_request: Option<i64>,
) -> Result<Session> {
    let mut tx = pool.begin().await?;
    if !consume_payment(&mut tx, deposit_txid, "session").await? {
        anyhow::bail!("This transaction has already been consumed");
    }
    let session = insert_session(
        &mut tx,
        merchant_id,
        deposit_txid,
        balance_zatoshis,
        refund_address,
        cost_per_request,
    )
    .await?;
    tx.commit().await?;
    Ok(session)
}

/// Consume the prepared address and its deposit in the same transaction.
pub async fn create_prepared_session(
    pool: &SqlitePool,
    merchant_id: &str,
    request_id: &str,
    deposit_txid: &str,
    balance: i64,
    refund_address: Option<&str>,
) -> Result<Session> {
    let mut tx = pool.begin().await?;
    let used = sqlx::query("UPDATE session_requests SET status = 'used' WHERE id = ? AND merchant_id = ? AND status = 'pending' AND expires_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now')")
        .bind(request_id).bind(merchant_id).execute(&mut *tx).await?;
    anyhow::ensure!(
        used.rows_affected() == 1,
        "Session request expired or already used"
    );
    anyhow::ensure!(
        consume_payment(&mut tx, deposit_txid, "session").await?,
        "Payment already consumed"
    );
    let session = insert_session(
        &mut tx,
        merchant_id,
        deposit_txid,
        balance,
        refund_address,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(session)
}

/// Durable replay tombstone; deliberately contains no merchant or bearer credential.
pub async fn consume_payment(
    conn: &mut sqlx::SqliteConnection,
    txid: &str,
    purpose: &str,
) -> Result<bool> {
    let result =
        sqlx::query("INSERT OR IGNORE INTO payment_consumptions (txid, purpose) VALUES (?, ?)")
            .bind(txid.to_ascii_lowercase())
            .bind(purpose)
            .execute(conn)
            .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn insert_session(
    conn: &mut sqlx::SqliteConnection,
    merchant_id: &str,
    deposit_txid: &str,
    balance: i64,
    refund_address: Option<&str>,
    cost: Option<i64>,
) -> Result<Session> {
    let cost = cost.unwrap_or(DEFAULT_COST_PER_REQUEST);
    anyhow::ensure!(
        cost > 0 && balance >= cost,
        "Invalid session balance or price"
    );
    let id = uuid::Uuid::new_v4().to_string();
    let token = generate_token();
    sqlx::query("INSERT INTO agent_sessions (id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, 'active', strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '+' || ? || ' hours'))")
        .bind(&id).bind(merchant_id).bind(deposit_txid.to_ascii_lowercase()).bind(&token)
        .bind(balance).bind(balance).bind(cost).bind(refund_address).bind(SESSION_EXPIRY_HOURS).execute(&mut *conn).await?;
    let row = sqlx::query_as::<_, SessionRow>("SELECT id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at, created_at, closed_at FROM agent_sessions WHERE id = ?")
        .bind(id).fetch_one(&mut *conn).await?;
    Ok(session_from_row(row))
}

pub async fn get_session(pool: &SqlitePool, session_id: &str) -> Result<Option<Session>> {
    let row = sqlx::query_as::<_, SessionRow>(
        "SELECT id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at, created_at, closed_at
         FROM agent_sessions WHERE id = ?"
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(session_from_row))
}

pub async fn get_session_by_token(
    pool: &SqlitePool,
    bearer_token: &str,
) -> Result<Option<Session>> {
    let row = sqlx::query_as::<_, SessionRow>(
        "SELECT id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at, created_at, closed_at
         FROM agent_sessions WHERE bearer_token = ?"
    )
    .bind(bearer_token)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(session_from_row))
}

/// A merchant may charge only its own session, at the requested integer price.
/// Reusing a request ID never authorizes another fulfillment or another debit.
pub async fn charge(
    pool: &SqlitePool,
    merchant_id: &str,
    token: &str,
    amount: i64,
    resource: &str,
    request_id: &str,
) -> Result<Option<Session>> {
    anyhow::ensure!(
        amount > 0
            && !resource.is_empty()
            && resource.len() <= 2048
            && !request_id.is_empty()
            && request_id.len() <= 128,
        "Invalid charge"
    );
    let mut tx = pool.begin().await?;
    let id: Option<String> = sqlx::query_scalar("UPDATE agent_sessions SET balance_remaining = balance_remaining - ?, requests_made = requests_made + 1
        WHERE bearer_token = ? AND merchant_id = ? AND status = 'active'
        AND balance_remaining >= ? AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now') RETURNING id")
        .bind(amount).bind(token).bind(merchant_id).bind(amount).fetch_optional(&mut *tx).await?;
    let Some(id) = id else {
        return Ok(None);
    };
    let inserted = sqlx::query("INSERT OR IGNORE INTO session_charges (session_id, request_id, resource, amount) VALUES (?, ?, ?, ?)")
        .bind(&id).bind(request_id).bind(resource).bind(amount).execute(&mut *tx).await?;
    if inserted.rows_affected() == 0 {
        return Ok(None);
    } // rollback the debit on duplicate
    let row = sqlx::query_as::<_, SessionRow>("SELECT id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at, created_at, closed_at FROM agent_sessions WHERE id = ?")
        .bind(id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(session_from_row(row)))
}

pub async fn close_session(pool: &SqlitePool, session_id: &str) -> Result<Option<SessionSummary>> {
    let session = get_session(pool, session_id).await?;
    let session = match session {
        Some(s) if s.status == "active" || s.status == "depleted" => s,
        Some(s) => {
            return Ok(Some(SessionSummary {
                session_id: s.id,
                requests_made: s.requests_made,
                balance_used: s.balance_zatoshis - s.balance_remaining,
                balance_remaining: s.balance_remaining,
                status: s.status,
                refund_address: s.refund_address,
            }));
        }
        None => return Ok(None),
    };

    sqlx::query(
        "UPDATE agent_sessions SET status = 'closed', closed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
    )
    .bind(session_id)
    .execute(pool)
    .await?;

    let balance_used = session.balance_zatoshis - session.balance_remaining;

    tracing::info!(
        session_id,
        requests = session.requests_made,
        balance_used,
        balance_remaining = session.balance_remaining,
        "Session closed"
    );

    Ok(Some(SessionSummary {
        session_id: session.id,
        requests_made: session.requests_made,
        balance_used,
        balance_remaining: session.balance_remaining,
        status: "closed".to_string(),
        refund_address: session.refund_address,
    }))
}

pub async fn get_summary(pool: &SqlitePool, session_id: &str) -> Result<Option<SessionSummary>> {
    let session = get_session(pool, session_id).await?;
    Ok(session.map(|s| SessionSummary {
        session_id: s.id,
        requests_made: s.requests_made,
        balance_used: s.balance_zatoshis - s.balance_remaining,
        balance_remaining: s.balance_remaining,
        status: s.status,
        refund_address: s.refund_address,
    }))
}

/// List sessions for a merchant (dashboard view)
pub async fn list_for_merchant(pool: &SqlitePool, merchant_id: &str) -> Result<Vec<Session>> {
    let rows = sqlx::query_as::<_, SessionRow>(
        "SELECT id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at, created_at, closed_at
         FROM agent_sessions WHERE merchant_id = ? ORDER BY created_at DESC LIMIT 100"
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(session_from_row).collect())
}

/// Check if a deposit txid has already been used for a session
pub async fn txid_already_used(pool: &SqlitePool, txid: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM payment_consumptions WHERE txid = lower(?)")
        .bind(txid)
        .fetch_one(pool)
        .await
        .unwrap_or(1)
        > 0
}

/// Create a session deposit request with a unique address (memo-free flow).
pub async fn create_session_request(
    pool: &SqlitePool,
    merchant_id: &str,
    merchant_uivk: &str,
) -> Result<SessionRequest> {
    let id = format!("sr_{}", uuid::Uuid::new_v4().simple());
    let div_index = crate::merchants::next_diversifier_index(pool, merchant_id).await?;
    let derived = crate::addresses::derive_invoice_address(merchant_uivk, div_index)?;

    sqlx::query(
        "INSERT INTO session_requests (id, merchant_id, deposit_address, diversifier_index, status, expires_at)
         VALUES (?, ?, ?, ?, 'pending', strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '+30 minutes'))"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(&derived.ua_string)
    .bind(div_index as i64)
    .execute(pool)
    .await?;

    Ok(SessionRequest {
        id,
        merchant_id: merchant_id.to_string(),
        deposit_address: derived.ua_string,
        diversifier_index: div_index,
    })
}

/// Look up a pending session request by ID.
pub async fn get_session_request(
    pool: &SqlitePool,
    request_id: &str,
) -> Result<Option<SessionRequest>> {
    let row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, merchant_id, deposit_address, diversifier_index
         FROM session_requests WHERE id = ? AND status = 'pending'
         AND expires_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| SessionRequest {
        id: r.0,
        merchant_id: r.1,
        deposit_address: r.2,
        diversifier_index: r.3 as u32,
    }))
}

pub struct SessionRequest {
    pub id: String,
    pub merchant_id: String,
    pub deposit_address: String,
    pub diversifier_index: u32,
}

fn generate_token() -> String {
    let uuid1 = uuid::Uuid::new_v4();
    let uuid2 = uuid::Uuid::new_v4();
    format!("cps_{}{}", uuid1.simple(), uuid2.simple())
}

fn session_from_row(row: SessionRow) -> Session {
    Session {
        id: row.0,
        merchant_id: row.1,
        deposit_txid: row.2,
        bearer_token: row.3,
        balance_zatoshis: row.4,
        balance_remaining: row.5,
        cost_per_request: row.6,
        requests_made: row.7,
        refund_address: row.8,
        status: row.9,
        expires_at: row.10,
        created_at: row.11,
        closed_at: row.12,
    }
}
