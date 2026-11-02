use futures::future::join_all;
use serde::Deserialize;

const BATCH_SIZE: usize = 20;

#[derive(Debug, Deserialize)]
struct MempoolResponse {
    transactions: Vec<MempoolTx>,
}

#[derive(Debug, Deserialize)]
struct MempoolTx {
    txid: String,
}

/// Fetches current mempool transaction IDs from ZecBlock API.
pub async fn fetch_mempool_txids(
    http: &reqwest::Client,
    api_url: &str,
) -> anyhow::Result<Vec<String>> {
    let resp: MempoolResponse = super::api::get(http, api_url, "mempool").await?;

    Ok(resp.transactions.into_iter().map(|tx| tx.txid).collect())
}

/// Fetches raw transaction hex from ZecBlock API.
pub async fn fetch_raw_tx(
    http: &reqwest::Client,
    api_url: &str,
    txid: &str,
) -> anyhow::Result<String> {
    let resp: serde_json::Value =
        super::api::get(http, api_url, &format!("transactions/{txid}/raw")).await?;

    resp["hex"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No hex field in raw tx response"))
}

/// Fetches raw transaction hex for multiple txids concurrently, in batches.
/// Returns (txid, hex) pairs for successful fetches.
pub async fn fetch_raw_txs_batch(
    http: &reqwest::Client,
    api_url: &str,
    txids: &[String],
) -> Vec<(String, String)> {
    let mut results = Vec::with_capacity(txids.len());

    for chunk in txids.chunks(BATCH_SIZE) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|txid| {
                let http = http.clone();
                async move {
                    fetch_raw_tx(&http, api_url, txid)
                        .await
                        .ok()
                        .map(|hex| (txid.clone(), hex))
                }
            })
            .collect();

        let batch_results = join_all(futures).await;
        results.extend(batch_results.into_iter().flatten());
    }

    results
}
