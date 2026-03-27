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
    let id = uuid::Uuid::new_v4().to_string();
    let bearer_token = generate_token();

    sqlx::query(
        "INSERT INTO sessions (id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, 'active', strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '+' || ? || ' hours'))"
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(deposit_txid)
    .bind(&bearer_token)
    .bind(balance_zatoshis)
    .bind(balance_zatoshis)
    .bind(DEFAULT_COST_PER_REQUEST)
    .bind(refund_address)
    .bind(SESSION_EXPIRY_HOURS)
    .execute(pool)
    .await?;

    let session = get_session(pool, &id).await?
        .ok_or_else(|| anyhow::anyhow!("Session creation failed"))?;

    tracing::info!(
        session_id = %id,
        merchant_id,
        deposit_txid,
        balance = balance_zatoshis,
        "Session created"
    );

    Ok(session)
}

pub async fn get_session(pool: &SqlitePool, session_id: &str) -> Result<Option<Session>> {
    let row = sqlx::query_as::<_, (String, String, String, String, i64, i64, i64, i64, Option<String>, String, String, String, Option<String>)>(
        "SELECT id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at, created_at, closed_at
         FROM sessions WHERE id = ?"
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| Session {
        id: r.0,
        merchant_id: r.1,
        deposit_txid: r.2,
        bearer_token: r.3,
        balance_zatoshis: r.4,
        balance_remaining: r.5,
        cost_per_request: r.6,
        requests_made: r.7,
        refund_address: r.8,
        status: r.9,
        expires_at: r.10,
        created_at: r.11,
        closed_at: r.12,
    }))
}

pub async fn validate_and_deduct(pool: &SqlitePool, bearer_token: &str) -> Result<Option<Session>> {
    // Expire stale sessions first
    sqlx::query("UPDATE sessions SET status = 'expired' WHERE status = 'active' AND expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')")
        .execute(pool).await.ok();

    let session = sqlx::query_as::<_, (String, String, i64, i64, i64, String, Option<String>)>(
        "SELECT id, merchant_id, balance_remaining, cost_per_request, requests_made, status, refund_address
         FROM sessions WHERE bearer_token = ? AND status = 'active'"
    )
    .bind(bearer_token)
    .fetch_optional(pool)
    .await?;

    let (id, merchant_id, remaining, cost, requests, _status, refund_address) = match session {
        Some(s) => s,
        None => return Ok(None),
    };

    if remaining < cost {
        sqlx::query("UPDATE sessions SET status = 'depleted' WHERE id = ?")
            .bind(&id).execute(pool).await.ok();
        return Ok(None);
    }

    let new_remaining = remaining - cost;
    let new_requests = requests + 1;

    sqlx::query(
        "UPDATE sessions SET balance_remaining = ?, requests_made = ? WHERE id = ?"
    )
    .bind(new_remaining)
    .bind(new_requests)
    .bind(&id)
    .execute(pool)
    .await?;

    Ok(Some(Session {
        id: id.clone(),
        merchant_id,
        deposit_txid: String::new(),
        bearer_token: bearer_token.to_string(),
        balance_zatoshis: 0,
        balance_remaining: new_remaining,
        cost_per_request: cost,
        requests_made: new_requests,
        refund_address,
        status: "active".to_string(),
        expires_at: String::new(),
        created_at: String::new(),
        closed_at: None,
    }))
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
        "UPDATE sessions SET status = 'closed', closed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
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
    let rows = sqlx::query_as::<_, (String, String, String, String, i64, i64, i64, i64, Option<String>, String, String, String, Option<String>)>(
        "SELECT id, merchant_id, deposit_txid, bearer_token, balance_zatoshis, balance_remaining, cost_per_request, requests_made, refund_address, status, expires_at, created_at, closed_at
         FROM sessions WHERE merchant_id = ? ORDER BY created_at DESC LIMIT 100"
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| Session {
        id: r.0,
        merchant_id: r.1,
        deposit_txid: r.2,
        bearer_token: r.3,
        balance_zatoshis: r.4,
        balance_remaining: r.5,
        cost_per_request: r.6,
        requests_made: r.7,
        refund_address: r.8,
        status: r.9,
        expires_at: r.10,
        created_at: r.11,
        closed_at: r.12,
    }).collect())
}

/// Check if a deposit txid has already been used for a session
pub async fn txid_already_used(pool: &SqlitePool, txid: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sessions WHERE deposit_txid = ?"
    )
    .bind(txid)
    .fetch_one(pool)
    .await
    .unwrap_or(0) > 0
}

fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let uuid1 = uuid::Uuid::new_v4();
    let uuid2 = uuid::Uuid::new_v4();
    format!("cps_{:x}{}{}", seed, uuid1.simple(), uuid2.simple())
}
