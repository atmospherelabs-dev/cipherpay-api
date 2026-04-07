use serde::Deserialize;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Debug, Deserialize)]
struct BlockchainInfoResponse {
    blocks: Option<u64>,
    headers: Option<u64>,
}

/// Simple circuit breaker for CipherScan API resilience.
/// Opens after `FAILURE_THRESHOLD` consecutive failures, stays open for
/// `COOLDOWN_SECS`, then transitions to half-open (allows one probe request).
pub struct CircuitBreaker {
    consecutive_failures: AtomicU32,
    opened_at_epoch_secs: AtomicU64,
}

const FAILURE_THRESHOLD: u32 = 5;
const COOLDOWN_SECS: u64 = 30;

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            opened_at_epoch_secs: AtomicU64::new(0),
        }
    }

    pub fn is_open(&self) -> bool {
        let failures = self.consecutive_failures.load(Ordering::Relaxed);
        if failures < FAILURE_THRESHOLD {
            return false;
        }
        let opened = self.opened_at_epoch_secs.load(Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now - opened < COOLDOWN_SECS
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        let prev = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        if prev + 1 >= FAILURE_THRESHOLD {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            self.opened_at_epoch_secs.store(now, Ordering::Relaxed);
            tracing::warn!(
                failures = prev + 1,
                cooldown_secs = COOLDOWN_SECS,
                "CipherScan circuit breaker OPEN"
            );
        }
    }
}

/// Gets the current chain tip height from CipherScan API.
pub async fn get_chain_height(http: &reqwest::Client, api_url: &str) -> anyhow::Result<u64> {
    let url = format!("{}/api/blockchain-info", api_url);
    let resp: BlockchainInfoResponse = http.get(&url).send().await?.json().await?;

    resp.blocks
        .or(resp.headers)
        .ok_or_else(|| anyhow::anyhow!("No block height in response"))
}

/// Fetches txids from a single block.
async fn fetch_single_block_txids(
    http: &reqwest::Client,
    api_url: &str,
    height: u64,
) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/api/block/{}", api_url, height);
    let resp: serde_json::Value = http
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch block {}: {}", height, e))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to parse block {} response: {}", height, e))?;

    extract_block_txids(&resp, height)
}

fn extract_block_txids(resp: &serde_json::Value, height: u64) -> anyhow::Result<Vec<String>> {
    let mut txids = Vec::new();
    if let Some(txs) = resp["transactions"].as_array() {
        for tx in txs {
            if let Some(txid) = tx["txid"].as_str() {
                txids.push(txid.to_string());
            }
        }
    } else if let Some(txs) = resp["tx"].as_array() {
        for tx in txs {
            if let Some(txid) = tx.as_str() {
                txids.push(txid.to_string());
            }
        }
    } else {
        return Err(anyhow::anyhow!(
            "Block {} response missing transaction list",
            height
        ));
    }
    Ok(txids)
}

const BLOCK_FETCH_BATCH_SIZE: usize = 10;

/// Fetches transaction IDs from a range of blocks in parallel batches.
pub async fn fetch_block_txids(
    http: &reqwest::Client,
    api_url: &str,
    start_height: u64,
    end_height: u64,
) -> anyhow::Result<Vec<String>> {
    let heights: Vec<u64> = (start_height..=end_height).collect();
    let mut all_txids = Vec::new();

    for chunk in heights.chunks(BLOCK_FETCH_BATCH_SIZE) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|&h| fetch_single_block_txids(http, api_url, h))
            .collect();
        let results = futures::future::join_all(futures).await;
        for txids in results {
            all_txids.extend(txids?);
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

    let confirmed = resp["block_height"].as_u64().is_some()
        || resp["blockHeight"].as_u64().is_some()
        || resp["confirmations"].as_u64().map_or(false, |c| c >= 1);

    Ok(confirmed)
}

#[cfg(test)]
mod tests {
    use super::extract_block_txids;

    #[test]
    fn extracts_txids_from_transactions_shape() {
        let resp = serde_json::json!({
            "transactions": [
                {"txid": "abc"},
                {"txid": "def"}
            ]
        });

        let txids = extract_block_txids(&resp, 100).unwrap();
        assert_eq!(txids, vec!["abc".to_string(), "def".to_string()]);
    }

    #[test]
    fn errors_when_transaction_list_missing() {
        let resp = serde_json::json!({
            "height": 100
        });

        let err = extract_block_txids(&resp, 100).unwrap_err();
        assert!(err.to_string().contains("missing transaction list"));
    }
}
