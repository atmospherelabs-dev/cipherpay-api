pub mod mempool;
pub mod blocks;
pub mod decrypt;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use sqlx::SqlitePool;

use crate::billing;
use crate::config::Config;
use crate::invoices;
use crate::invoices::matching;
use crate::webhooks;

pub type SeenTxids = Arc<RwLock<HashMap<String, Instant>>>;

const SEEN_TXID_TTL_SECS: u64 = 3600; // 1 hour
const SEEN_TXID_EVICT_INTERVAL: u64 = 300; // run eviction every 5 minutes

/// Pre-computed decryption keys for all merchants, refreshed when the merchant set changes.
struct KeyCache {
    keys: Vec<(String, decrypt::CachedKeys)>,
    merchant_ids: Vec<String>,
}

pub async fn run(config: Config, pool: SqlitePool, http: reqwest::Client) {
    let seen_txids: SeenTxids = Arc::new(RwLock::new(HashMap::new()));

    let persisted_height = crate::db::get_scanner_state(&pool, "last_height").await
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(h) = persisted_height {
        tracing::info!(height = h, "Resumed scanner from persisted block height");
    }
    let last_height: Arc<RwLock<Option<u64>>> = Arc::new(RwLock::new(persisted_height));

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
        let mut key_cache: Option<KeyCache> = None;
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(mempool_config.mempool_poll_interval_secs),
        );
        loop {
            interval.tick().await;
            if let Err(e) = scan_mempool(&mempool_config, &mempool_pool, &mempool_http, &mempool_seen, &mut key_cache).await {
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
        let mut key_cache: Option<KeyCache> = None;
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(block_config.block_poll_interval_secs),
        );
        loop {
            interval.tick().await;
            let _ = invoices::expire_old_invoices(&block_pool).await;

            if let Err(e) = scan_blocks(&block_config, &block_pool, &block_http, &block_seen, &last_height, &mut key_cache).await {
                tracing::error!(error = %e, "Block scan error");
            }
        }
    });

    let evict_seen = seen_txids.clone();
    let evict_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(SEEN_TXID_EVICT_INTERVAL),
        );
        loop {
            interval.tick().await;
            let cutoff = Instant::now() - std::time::Duration::from_secs(SEEN_TXID_TTL_SECS);
            let mut set = evict_seen.write().await;
            let before = set.len();
            set.retain(|_, ts| *ts > cutoff);
            let evicted = before - set.len();
            if evicted > 0 {
                tracing::debug!(evicted, remaining = set.len(), "Evicted stale seen_txids");
            }
        }
    });

    let _ = tokio::join!(mempool_handle, block_handle, evict_handle);
}

/// Build or refresh the PIVK cache when the merchant set changes.
/// Compares merchant IDs (not just count) so additions, deletions,
/// or replacements all trigger a rebuild.
fn refresh_key_cache<'a>(
    cache: &'a mut Option<KeyCache>,
    merchants: &[crate::merchants::Merchant],
) -> &'a [(String, decrypt::CachedKeys)] {
    let current_ids: Vec<String> = merchants.iter().map(|m| m.id.clone()).collect();

    let needs_refresh = match cache {
        Some(c) => c.merchant_ids != current_ids,
        None => true,
    };

    if needs_refresh {
        let mut keys = Vec::with_capacity(merchants.len());
        for m in merchants {
            match decrypt::prepare_keys(&m.ufvk) {
                Ok(k) => keys.push((m.id.clone(), k)),
                Err(e) => tracing::warn!(merchant_id = %m.id, error = %e, "Failed to prepare PIVK"),
            }
        }
        tracing::info!(merchants = keys.len(), "PIVK cache refreshed");
        *cache = Some(KeyCache { merchant_ids: current_ids, keys });
    }

    &cache.as_ref().unwrap().keys
}

/// Fire a webhook without blocking the scan loop.
fn spawn_webhook(pool: &SqlitePool, http: &reqwest::Client, invoice_id: &str, event: &str, txid: &str) {
    let pool = pool.clone();
    let http = http.clone();
    let invoice_id = invoice_id.to_string();
    let event = event.to_string();
    let txid = txid.to_string();
    tokio::spawn(async move {
        if let Err(e) = webhooks::dispatch(&pool, &http, &invoice_id, &event, &txid).await {
            tracing::error!(invoice_id, event, error = %e, "Async webhook failed");
        }
    });
}

/// Fire a payment webhook without blocking the scan loop.
fn spawn_payment_webhook(
    pool: &SqlitePool, http: &reqwest::Client,
    invoice_id: &str, event: &str, txid: &str,
    price_zatoshis: i64, received_zatoshis: i64, overpaid: bool,
) {
    let pool = pool.clone();
    let http = http.clone();
    let invoice_id = invoice_id.to_string();
    let event = event.to_string();
    let txid = txid.to_string();
    tokio::spawn(async move {
        if let Err(e) = webhooks::dispatch_payment(
            &pool, &http, &invoice_id, &event, &txid,
            price_zatoshis, received_zatoshis, overpaid,
        ).await {
            tracing::error!(invoice_id, event, error = %e, "Async payment webhook failed");
        }
    });
}

async fn scan_mempool(
    config: &Config,
    pool: &SqlitePool,
    http: &reqwest::Client,
    seen: &SeenTxids,
    key_cache: &mut Option<KeyCache>,
) -> anyhow::Result<()> {
    let pending = invoices::get_pending_invoices(pool).await?;
    if pending.is_empty() {
        return Ok(());
    }

    let merchants = crate::merchants::get_all_merchants(pool, &config.encryption_key).await?;
    if merchants.is_empty() {
        return Ok(());
    }

    let cached_keys = refresh_key_cache(key_cache, &merchants);

    let mempool_txids = mempool::fetch_mempool_txids(http, &config.cipherscan_api_url).await?;

    let new_txids: Vec<String> = {
        let seen_set = seen.read().await;
        mempool_txids.into_iter().filter(|txid| !seen_set.contains_key(txid)).collect()
    };

    if new_txids.is_empty() {
        return Ok(());
    }

    tracing::debug!(count = new_txids.len(), "New mempool transactions");

    {
        let mut seen_set = seen.write().await;
        let now = Instant::now();
        for txid in &new_txids {
            seen_set.insert(txid.clone(), now);
        }
    }

    let raw_txs = mempool::fetch_raw_txs_batch(http, &config.cipherscan_api_url, &new_txids).await;
    tracing::debug!(fetched = raw_txs.len(), total = new_txids.len(), "Batch fetched raw txs");

    for (txid, raw_hex) in &raw_txs {
        // Aggregate all outputs per invoice across all merchants in this tx
        let mut invoice_totals: HashMap<String, (invoices::Invoice, i64)> = HashMap::new();

        for (_merchant_id, keys) in cached_keys {
            match decrypt::try_decrypt_with_keys(raw_hex, keys) {
                Ok(outputs) => {
                    for output in &outputs {
                        let recipient_hex = hex::encode(output.recipient_raw);
                        tracing::info!(txid, memo = %output.memo, amount = output.amount_zec, "Decrypted mempool tx");

                        if let Some(invoice) = matching::find_matching_invoice(&pending, &recipient_hex, &output.memo) {
                            let entry = invoice_totals.entry(invoice.id.clone())
                                .or_insert((invoice.clone(), 0));
                            entry.1 += output.amount_zatoshis as i64;
                        }
                    }
                }
                Err(_) => {}
            }
        }

        for (invoice_id, (invoice, tx_total)) in &invoice_totals {
            let dust_min = std::cmp::max(
                (invoice.price_zatoshis as f64 * decrypt::DUST_THRESHOLD_FRACTION) as i64,
                decrypt::DUST_THRESHOLD_MIN_ZATOSHIS,
            );
            if *tx_total < dust_min && *tx_total < invoice.price_zatoshis {
                tracing::debug!(invoice_id, tx_total, dust_min, "Ignoring dust payment");
                continue;
            }

            let new_received = if invoice.status == "underpaid" {
                invoices::accumulate_payment(pool, invoice_id, *tx_total).await?
            } else {
                *tx_total
            };

            let min = (invoice.price_zatoshis as f64 * decrypt::SLIPPAGE_TOLERANCE) as i64;

            if new_received >= min {
                let changed = invoices::mark_detected(pool, invoice_id, txid, new_received).await?;
                if changed {
                    let overpaid = new_received > invoice.price_zatoshis + 1000;
                    spawn_payment_webhook(pool, http, invoice_id, "detected", txid,
                        invoice.price_zatoshis, new_received, overpaid);
                    try_detect_fee(pool, config, raw_hex, invoice_id).await;
                }
            } else if invoice.status == "pending" {
                invoices::mark_underpaid(pool, invoice_id, new_received, txid).await?;
                spawn_payment_webhook(pool, http, invoice_id, "underpaid", txid,
                    invoice.price_zatoshis, new_received, false);
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
    key_cache: &mut Option<KeyCache>,
) -> anyhow::Result<()> {
    let pending = invoices::get_pending_invoices(pool).await?;
    if pending.is_empty() {
        return Ok(());
    }

    let detected: Vec<_> = pending.iter().filter(|i| i.status == "detected").cloned().collect();
    for invoice in &detected {
        if let Some(txid) = &invoice.detected_txid {
            match blocks::check_tx_confirmed(http, &config.cipherscan_api_url, txid).await {
                Ok(true) => {
                    let changed = invoices::mark_confirmed(pool, &invoice.id).await?;
                    if changed {
                        spawn_webhook(pool, http, &invoice.id, "confirmed", txid);
                        on_invoice_confirmed(pool, config, invoice).await;
                    }
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
        let cached_keys = refresh_key_cache(key_cache, &merchants);
        let block_txids = blocks::fetch_block_txids(http, &config.cipherscan_api_url, start_height, current_height).await?;

        for txid in &block_txids {
            if seen.read().await.contains_key(txid) {
                continue;
            }

            let raw_hex = match mempool::fetch_raw_tx(http, &config.cipherscan_api_url, txid).await {
                Ok(hex) => hex,
                Err(_) => continue,
            };

            let mut invoice_totals: HashMap<String, (invoices::Invoice, i64)> = HashMap::new();
            for (_merchant_id, keys) in cached_keys.iter() {
                if let Ok(outputs) = decrypt::try_decrypt_with_keys(&raw_hex, keys) {
                    for output in &outputs {
                        let recipient_hex = hex::encode(output.recipient_raw);
                        if let Some(invoice) = matching::find_matching_invoice(&pending, &recipient_hex, &output.memo) {
                            let entry = invoice_totals.entry(invoice.id.clone())
                                .or_insert((invoice.clone(), 0));
                            entry.1 += output.amount_zatoshis as i64;
                        }
                    }
                }
            }

            for (invoice_id, (invoice, tx_total)) in &invoice_totals {
                let dust_min = std::cmp::max(
                    (invoice.price_zatoshis as f64 * decrypt::DUST_THRESHOLD_FRACTION) as i64,
                    decrypt::DUST_THRESHOLD_MIN_ZATOSHIS,
                );
                if *tx_total < dust_min && *tx_total < invoice.price_zatoshis {
                    tracing::debug!(invoice_id, tx_total, dust_min, "Ignoring dust payment in block");
                    continue;
                }

                let new_received = if invoice.status == "underpaid" {
                    invoices::accumulate_payment(pool, invoice_id, *tx_total).await?
                } else {
                    *tx_total
                };

                let min = (invoice.price_zatoshis as f64 * decrypt::SLIPPAGE_TOLERANCE) as i64;

                if new_received >= min && (invoice.status == "pending" || invoice.status == "underpaid") {
                    let detected = invoices::mark_detected(pool, invoice_id, txid, new_received).await?;
                    if detected {
                        let confirmed = invoices::mark_confirmed(pool, invoice_id).await?;
                        if confirmed {
                            let overpaid = new_received > invoice.price_zatoshis + 1000;
                            spawn_payment_webhook(pool, http, invoice_id, "confirmed", txid,
                                invoice.price_zatoshis, new_received, overpaid);
                            on_invoice_confirmed(pool, config, invoice).await;
                        }
                        try_detect_fee(pool, config, &raw_hex, invoice_id).await;
                    }
                } else if new_received < min && invoice.status == "pending" {
                    invoices::mark_underpaid(pool, invoice_id, new_received, txid).await?;
                    spawn_payment_webhook(pool, http, invoice_id, "underpaid", txid,
                        invoice.price_zatoshis, new_received, false);
                }
            }

            seen.write().await.insert(txid.clone(), Instant::now());
        }
    }

    *last_height.write().await = Some(current_height);
    if let Err(e) = crate::db::set_scanner_state(pool, "last_height", &current_height.to_string()).await {
        tracing::warn!(error = %e, "Failed to persist last_height");
    }
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
