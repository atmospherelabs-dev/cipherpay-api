use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BlockchainInfoResponse {
    blocks: Option<u64>,
    headers: Option<u64>,
}

/// Gets the current chain tip height from CipherScan API.
pub async fn get_chain_height(
    http: &reqwest::Client,
    api_url: &str,
) -> anyhow::Result<u64> {
    let url = format!("{}/api/blockchain-info", api_url);
    let resp: BlockchainInfoResponse = http.get(&url).send().await?.json().await?;

    resp.blocks
        .or(resp.headers)
        .ok_or_else(|| anyhow::anyhow!("No block height in response"))
}

/// Fetches transaction IDs from a range of blocks.
pub async fn fetch_block_txids(
    http: &reqwest::Client,
    api_url: &str,
    start_height: u64,
    end_height: u64,
) -> anyhow::Result<Vec<String>> {
    let mut all_txids = Vec::new();

    for height in start_height..=end_height {
        let url = format!("{}/api/block/{}", api_url, height);
        let resp: serde_json::Value = match http.get(&url).send().await {
            Ok(r) => r.json().await?,
            Err(e) => {
                tracing::warn!(height, error = %e, "Failed to fetch block");
                continue;
            }
        };

        // Extract txids from block response
        if let Some(txs) = resp["transactions"].as_array() {
            for tx in txs {
                if let Some(txid) = tx["txid"].as_str() {
                    all_txids.push(txid.to_string());
                }
            }
        } else if let Some(txs) = resp["tx"].as_array() {
            for tx in txs {
                if let Some(txid) = tx.as_str() {
                    all_txids.push(txid.to_string());
                }
            }
        }
    }

    Ok(all_txids)
}

/// Checks if a transaction has been confirmed (included in a block).
pub async fn check_tx_confirmed(
    http: &reqwest::Client,
    api_url: &str,
    txid: &str,
) -> anyhow::Result<bool> {
    let url = format!("{}/api/tx/{}", api_url, txid);
    let resp: serde_json::Value = http.get(&url).send().await?.json().await?;

    // If the tx has a block_height field, it's confirmed
    let confirmed = resp["block_height"].as_u64().is_some()
        || resp["blockHeight"].as_u64().is_some()
        || resp["confirmations"].as_u64().map_or(false, |c| c >= 1);

    Ok(confirmed)
}
