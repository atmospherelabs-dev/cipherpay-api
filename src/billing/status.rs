use sqlx::SqlitePool;

pub async fn get_merchant_billing_status(
    pool: &SqlitePool,
    merchant_id: &str,
) -> anyhow::Result<String> {
    let status: String =
        sqlx::query_scalar("SELECT COALESCE(billing_status, 'active') FROM merchants WHERE id = ?")
            .bind(merchant_id)
            .fetch_one(pool)
            .await?;
    Ok(status)
}
