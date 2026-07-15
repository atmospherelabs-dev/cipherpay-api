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
    let response = http.get(&url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "CipherScan API returned HTTP {} for blockchain-info",
            status.as_u16()
        ));
    }
    let resp: BlockchainInfoResponse = response.json().await.map_err(|e| {
        anyhow::anyhow!("Failed to parse blockchain-info JSON: {e}")
    })?;

    resp.blocks
        .or(resp.headers)
        .ok_or_else(|| anyhow::anyhow!("No block height in response"))
}

const BLOCK_FETCH_RETRIES: u32 = 3;
const BLOCK_FETCH_BASE_DELAY_MS: u64 = 1000;

/// Fetches txids from a single block with retry + exponential backoff.
async fn fetch_single_block_txids(
    http: &reqwest::Client,
    api_url: &str,
    height: u64,
) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/api/block/{}", api_url, height);
    let mut last_err = None;

    for attempt in 0..BLOCK_FETCH_RETRIES {
        if attempt > 0 {
            let delay = BLOCK_FETCH_BASE_DELAY_MS * (1 << (attempt - 1));
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }

        let resp = match http.get(&url).send().await {
            Ok(r) => {
                let status = r.status();
                if !status.is_success() {
                    last_err = Some(anyhow::anyhow!(
                        "CipherScan API returned HTTP {} for block {}",
                        status.as_u16(), height
                    ));
                    continue;
                }
                match r.json::<serde_json::Value>().await {
                    Ok(v) => v,
                    Err(e) => {
                        last_err = Some(anyhow::anyhow!("Failed to parse block {} response: {}", height, e));
                        continue;
                    }
                }
            },
            Err(e) => {
                last_err = Some(anyhow::anyhow!("Failed to fetch block {}: {}", height, e));
                continue;
            }
        };

        match extract_block_txids(&resp, height) {
            Ok(txids) => return Ok(txids),
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Block {} fetch failed after retries", height)))
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
    // Every block above genesis has at least a coinbase transaction
    if txids.is_empty() && height > 0 {
        return Err(anyhow::anyhow!(
            "Block {} returned empty transaction list (expected at least coinbase)",
            height
        ));
    }
    Ok(txids)
}

const BLOCK_FETCH_BATCH_SIZE: usize = 10;

/// Fetches transaction IDs from a range of blocks in parallel batches.
/// Individual block failures are isolated — other blocks in the batch still
/// get processed. Returns an error only if ANY block failed (so the caller
/// knows not to advance last_height past the failed block).
pub async fn fetch_block_txids(
    http: &reqwest::Client,
    api_url: &str,
    start_height: u64,
    end_height: u64,
) -> anyhow::Result<Vec<String>> {
    let heights: Vec<u64> = (start_height..=end_height).collect();
    let mut all_txids = Vec::new();
    let mut first_failed_height: Option<u64> = None;
    let mut last_error: Option<anyhow::Error> = None;

    for chunk in heights.chunks(BLOCK_FETCH_BATCH_SIZE) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|&h| async move { (h, fetch_single_block_txids(http, api_url, h).await) })
            .collect();
        let results = futures::future::join_all(futures).await;
        for (h, result) in results {
            match result {
                Ok(txids) => all_txids.extend(txids),
                Err(e) => {
                    tracing::error!(height = h, error = %e, "Block fetch failed after retries");
                    if first_failed_height.is_none() {
                        first_failed_height = Some(h);
                    }
                    last_error = Some(e);
                }
            }
        }
    }

    if let Some(e) = last_error {
        return Err(e);
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
    let response = http.get(&url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "CipherScan API returned HTTP {} for tx {}",
            status.as_u16(), txid
        ));
    }
    let resp: serde_json::Value = response.json().await?;

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

    #[test]
    fn errors_when_transaction_list_empty() {
        let resp = serde_json::json!({
            "transactions": []
        });

        let err = extract_block_txids(&resp, 100).unwrap_err();
        assert!(err.to_string().contains("empty transaction list"));
    }

    #[test]
    fn allows_empty_genesis_block() {
        let resp = serde_json::json!({
            "transactions": []
        });

        let txids = extract_block_txids(&resp, 0).unwrap();
        assert!(txids.is_empty());
    }
}
