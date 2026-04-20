use sqlx::SqlitePool;

/// Periodic data purge: cleans up expired sessions, old webhook deliveries,
/// expired recovery tokens, and optionally old expired/refunded invoices.
pub async fn run_data_purge(pool: &SqlitePool, purge_days: i64) -> anyhow::Result<()> {
    let cutoff = format!("-{} days", purge_days);

    let agent_sessions_purged = sqlx::query(
        "DELETE FROM agent_sessions WHERE
            expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            OR (status IN ('closed', 'depleted') AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?))"
    )
    .bind(&cutoff)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);

    let _ = sqlx::query(
        "DELETE FROM session_requests WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .execute(pool)
    .await;

    let _ = sqlx::query(
        "DELETE FROM sessions WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .execute(pool)
    .await;

    let tokens = sqlx::query(
        "DELETE FROM recovery_tokens WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .execute(pool)
    .await?;

    let webhooks = sqlx::query(
        "DELETE FROM webhook_deliveries WHERE status IN ('delivered', 'failed')
         AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?;

    let tickets = sqlx::query(
        "DELETE FROM tickets
         WHERE status = 'void'
         AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?;

    let passkey_challenges = crate::api::passkey::purge_expired_challenges(pool)
        .await
        .unwrap_or(0);

    let total = agent_sessions_purged
        + tokens.rows_affected()
        + webhooks.rows_affected()
        + tickets.rows_affected()
        + passkey_challenges;
    if total > 0 {
        tracing::info!(
            agent_sessions = agent_sessions_purged,
            tokens = tokens.rows_affected(),
            webhooks = webhooks.rows_affected(),
            tickets = tickets.rows_affected(),
            passkey_challenges = passkey_challenges,
            "Data purge completed"
        );
    }
    Ok(())
}
