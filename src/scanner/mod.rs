pub mod mempool;
pub mod blocks;
pub mod decrypt;

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use sqlx::SqlitePool;

use crate::billing;
use crate::config::Config;
use crate::invoices;
use crate::invoices::matching;
use crate::webhooks;

pub type SeenTxids = Arc<RwLock<HashSet<String>>>;

pub async fn run(config: Config, pool: SqlitePool, http: reqwest::Client) {
    let seen_txids: SeenTxids = Arc::new(RwLock::new(HashSet::new()));
    let last_height: Arc<RwLock<Option<u64>>> = Arc::new(RwLock::new(None));

    tracing::info!(
        api = %config.cipherscan_api_url,
        mempool_interval = config.mempool_poll_interval_secs,
        block_interval = config.block_poll_interval_secs,
        "Scanner started"
    );

    let mempool_config = config.clone();
    let mempool_pool = pool.clone();
    let mempool_http = http.clone();
    let mempool_seen = seen_txids.clone();

    let mempool_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(mempool_config.mempool_poll_interval_secs),
        );
        loop {
            interval.tick().await;
            if let Err(e) = scan_mempool(&mempool_config, &mempool_pool, &mempool_http, &mempool_seen).await {
                tracing::error!(error = %e, "Mempool scan error");
            }

            if mempool_config.fee_enabled() {
                let _ = billing::check_settlement_payments(&mempool_pool).await;
            }
        }
    });

    let block_config = config.clone();
    let block_pool = pool.clone();
    let block_http = http.clone();
    let block_seen = seen_txids.clone();

    let block_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(block_config.block_poll_interval_secs),
        );
        loop {
            interval.tick().await;
            let _ = invoices::expire_old_invoices(&block_pool).await;

            if let Err(e) = scan_blocks(&block_config, &block_pool, &block_http, &block_seen, &last_height).await {
                tracing::error!(error = %e, "Block scan error");
            }
        }
    });

    let _ = tokio::join!(mempool_handle, block_handle);
}

async fn scan_mempool(
    config: &Config,
    pool: &SqlitePool,
    http: &reqwest::Client,
    seen: &SeenTxids,
) -> anyhow::Result<()> {
    let pending = invoices::get_pending_invoices(pool).await?;
    if pending.is_empty() {
        return Ok(());
    }

    let merchants = crate::merchants::get_all_merchants(pool, &config.encryption_key).await?;
    if merchants.is_empty() {
        return Ok(());
    }

    let mempool_txids = mempool::fetch_mempool_txids(http, &config.cipherscan_api_url).await?;

    let new_txids: Vec<String> = {
        let seen_set = seen.read().await;
        mempool_txids.into_iter().filter(|txid| !seen_set.contains(txid)).collect()
    };

    if new_txids.is_empty() {
        return Ok(());
    }

    tracing::debug!(count = new_txids.len(), "New mempool transactions");

    {
        let mut seen_set = seen.write().await;
        for txid in &new_txids {
            seen_set.insert(txid.clone());
        }
    }

    let raw_txs = mempool::fetch_raw_txs_batch(http, &config.cipherscan_api_url, &new_txids).await;
    tracing::debug!(fetched = raw_txs.len(), total = new_txids.len(), "Batch fetched raw txs");

    for (txid, raw_hex) in &raw_txs {
        for merchant in &merchants {
            match decrypt::try_decrypt_all_outputs(raw_hex, &merchant.ufvk) {
                Ok(outputs) => {
                    for output in &outputs {
                        let recipient_hex = hex::encode(output.recipient_raw);
                        tracing::info!(txid, memo = %output.memo, amount = output.amount_zec, "Decrypted mempool tx");

                        if let Some(invoice) = matching::find_matching_invoice(&pending, &recipient_hex, &output.memo) {
                            let new_received = if invoice.status == "underpaid" {
                                invoices::accumulate_payment(pool, &invoice.id, output.amount_zatoshis as i64).await?
                            } else {
                                output.amount_zatoshis as i64
                            };

                            let min = (invoice.price_zatoshis as f64 * decrypt::SLIPPAGE_TOLERANCE) as i64;

                            if new_received >= min {
                                invoices::mark_detected(pool, &invoice.id, txid, new_received).await?;
                                let overpaid = new_received > invoice.price_zatoshis;
                                webhooks::dispatch_payment(pool, http, &invoice.id, "detected", txid,
                                    invoice.price_zatoshis, new_received, overpaid).await?;
                                try_detect_fee(pool, config, raw_hex, &invoice.id).await;
                            } else if invoice.status == "pending" {
                                invoices::mark_underpaid(pool, &invoice.id, new_received, txid).await?;
                                webhooks::dispatch_payment(pool, http, &invoice.id, "underpaid", txid,
                                    invoice.price_zatoshis, new_received, false).await?;
                            }
                            // if already underpaid and still not enough, accumulate_payment already extended timer
                        }
                    }
                }
                Err(_) => {}
            }
        }
    }

    Ok(())
}

async fn scan_blocks(
    config: &Config,
    pool: &SqlitePool,
    http: &reqwest::Client,
    seen: &SeenTxids,
    last_height: &Arc<RwLock<Option<u64>>>,
) -> anyhow::Result<()> {
    let pending = invoices::get_pending_invoices(pool).await?;
    if pending.is_empty() {
        return Ok(());
    }

    // Check detected -> confirmed transitions
    let detected: Vec<_> = pending.iter().filter(|i| i.status == "detected").cloned().collect();
    for invoice in &detected {
        if let Some(txid) = &invoice.detected_txid {
            match blocks::check_tx_confirmed(http, &config.cipherscan_api_url, txid).await {
                Ok(true) => {
                    invoices::mark_confirmed(pool, &invoice.id).await?;
                    webhooks::dispatch(pool, http, &invoice.id, "confirmed", txid).await?;
                    on_invoice_confirmed(pool, config, invoice).await;
                }
                Ok(false) => {}
                Err(e) => tracing::debug!(txid, error = %e, "Confirmation check failed"),
            }
        }
    }

    let current_height = blocks::get_chain_height(http, &config.cipherscan_api_url).await?;
    let start_height = {
        let last = last_height.read().await;
        match *last {
            Some(h) => h + 1,
            None => current_height,
        }
    };

    if start_height <= current_height && start_height < current_height {
        let merchants = crate::merchants::get_all_merchants(pool, &config.encryption_key).await?;
        let block_txids = blocks::fetch_block_txids(http, &config.cipherscan_api_url, start_height, current_height).await?;

        for txid in &block_txids {
            if seen.read().await.contains(txid) {
                continue;
            }

            let raw_hex = match mempool::fetch_raw_tx(http, &config.cipherscan_api_url, txid).await {
                Ok(hex) => hex,
                Err(_) => continue,
            };

            for merchant in &merchants {
                if let Ok(outputs) = decrypt::try_decrypt_all_outputs(&raw_hex, &merchant.ufvk) {
                    for output in &outputs {
                        let recipient_hex = hex::encode(output.recipient_raw);
                        if let Some(invoice) = matching::find_matching_invoice(&pending, &recipient_hex, &output.memo) {
                            let new_received = if invoice.status == "underpaid" {
                                invoices::accumulate_payment(pool, &invoice.id, output.amount_zatoshis as i64).await?
                            } else {
                                output.amount_zatoshis as i64
                            };

                            let min = (invoice.price_zatoshis as f64 * decrypt::SLIPPAGE_TOLERANCE) as i64;

                            if new_received >= min && (invoice.status == "pending" || invoice.status == "underpaid") {
                                invoices::mark_detected(pool, &invoice.id, txid, new_received).await?;
                                invoices::mark_confirmed(pool, &invoice.id).await?;
                                let overpaid = new_received > invoice.price_zatoshis;
                                webhooks::dispatch_payment(pool, http, &invoice.id, "confirmed", txid,
                                    invoice.price_zatoshis, new_received, overpaid).await?;
                                on_invoice_confirmed(pool, config, invoice).await;
                                try_detect_fee(pool, config, &raw_hex, &invoice.id).await;
                            } else if new_received < min && invoice.status == "pending" {
                                invoices::mark_underpaid(pool, &invoice.id, new_received, txid).await?;
                                webhooks::dispatch_payment(pool, http, &invoice.id, "underpaid", txid,
                                    invoice.price_zatoshis, new_received, false).await?;
                            }
                        }
                    }
                }
            }
            seen.write().await.insert(txid.clone());
        }
    }

    *last_height.write().await = Some(current_height);
    Ok(())
}

/// When an invoice is confirmed, create a fee ledger entry and ensure a billing cycle exists.
async fn on_invoice_confirmed(pool: &SqlitePool, config: &Config, invoice: &invoices::Invoice) {
    if !config.fee_enabled() {
        return;
    }

    let fee_amount = invoice.price_zec * config.fee_rate;
    if fee_amount < 0.00000001 {
        return;
    }

    if let Err(e) = billing::ensure_billing_cycle(pool, &invoice.merchant_id, config).await {
        tracing::error!(error = %e, "Failed to ensure billing cycle");
    }

    if let Err(e) = billing::create_fee_entry(pool, &invoice.id, &invoice.merchant_id, fee_amount).await {
        tracing::error!(error = %e, "Failed to create fee entry");
    }
}

/// After a merchant payment is detected, try to decrypt the same tx against
/// the CipherPay fee UFVK to check if the fee output was included (ZIP 321).
async fn try_detect_fee(pool: &SqlitePool, config: &Config, raw_hex: &str, invoice_id: &str) {
    let fee_ufvk = match &config.fee_ufvk {
        Some(u) => u,
        None => return,
    };

    let fee_memo_prefix = format!("FEE-{}", invoice_id);

    match decrypt::try_decrypt_all_outputs(raw_hex, fee_ufvk) {
        Ok(outputs) => {
            for output in &outputs {
                if output.memo.starts_with(&fee_memo_prefix) {
                    tracing::info!(
                        invoice_id,
                        fee_zec = output.amount_zec,
                        "Fee auto-collected via ZIP 321"
                    );
                    let _ = billing::mark_fee_collected(pool, invoice_id).await;
                    return;
                }
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "Fee UFVK decryption failed (non-critical)");
        }
    }
}
