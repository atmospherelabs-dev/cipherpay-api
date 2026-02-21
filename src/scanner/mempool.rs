use futures::future::join_all;
use serde::Deserialize;

const BATCH_SIZE: usize = 20;

#[derive(Debug, Deserialize)]
struct MempoolResponse {
    transactions: Option<Vec<MempoolTx>>,
}

#[derive(Debug, Deserialize)]
struct MempoolTx {
    txid: String,
}

/// Fetches current mempool transaction IDs from CipherScan API.
pub async fn fetch_mempool_txids(
    http: &reqwest::Client,
    api_url: &str,
) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/api/mempool", api_url);
    let resp: MempoolResponse = http.get(&url).send().await?.json().await?;

    Ok(resp
        .transactions
        .unwrap_or_default()
        .into_iter()
        .map(|tx| tx.txid)
        .collect())
}

/// Fetches raw transaction hex from CipherScan API.
pub async fn fetch_raw_tx(
    http: &reqwest::Client,
    api_url: &str,
    txid: &str,
) -> anyhow::Result<String> {
    let url = format!("{}/api/tx/{}/raw", api_url, txid);
    let resp: serde_json::Value = http.get(&url).send().await?.json().await?;

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
        let futures: Vec<_> = chunk.iter().map(|txid| {
            let http = http.clone();
            let url = format!("{}/api/tx/{}/raw", api_url, txid);
            let txid = txid.clone();
            async move {
                let resp: Result<serde_json::Value, _> = async {
                    Ok(http.get(&url).send().await?.json().await?)
                }.await;

                match resp {
                    Ok(val) => val["hex"]
                        .as_str()
                        .map(|hex| (txid, hex.to_string())),
                    Err::<_, anyhow::Error>(_) => None,
                }
            }
        }).collect();

        let batch_results = join_all(futures).await;
        results.extend(batch_results.into_iter().flatten());
    }

    results
}
