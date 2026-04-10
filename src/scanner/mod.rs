pub mod blocks;
pub mod decrypt;
mod invoice_detection;
pub mod mempool;
pub mod ws;

use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

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
    let circuit_breaker = Arc::new(blocks::CircuitBreaker::new());
    let seen_txids: SeenTxids = Arc::new(RwLock::new(HashMap::new()));

    let persisted_height = crate::db::get_scanner_state(&pool, "last_height")
        .await
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

    // Spawn WS client if service key is configured
    let mut ws_rx: Option<tokio::sync::mpsc::Receiver<ws::MempoolPush>> = None;
    if let Some(ref key) = config.cipherscan_service_key {
        let ws_url = ws::api_url_to_ws(&config.cipherscan_api_url);
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        ws_rx = Some(rx);
        let ws_key = key.clone();
        tokio::spawn(async move {
            ws::run(ws_url, ws_key, tx).await;
        });
    }

    let mempool_config = config.clone();
    let mempool_pool = pool.clone();
    let mempool_http = http.clone();
    let mempool_seen = seen_txids.clone();
    let mempool_cb = circuit_breaker.clone();
    let has_ws = ws_rx.is_some();

    let mempool_handle = tokio::spawn(async move {
        let mut key_cache: Option<KeyCache> = None;
        let mut ws_receiver = ws_rx;

        // With WS: poll every 30s as a slow fallback. Without: use configured interval.
        let poll_secs = if has_ws {
            30
        } else {
            mempool_config.mempool_poll_interval_secs
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));

        if has_ws {
            tracing::info!(
                poll_fallback_secs = poll_secs,
                "Mempool: WebSocket mode + polling fallback"
            );
        }

        loop {
            tokio::select! {
                result = async {
                    match ws_receiver.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(push) => {
                            {
                                let mut seen_set = mempool_seen.write().await;
                                seen_set.insert(push.txid.clone(), Instant::now());
                            }
                            if let Err(e) = process_ws_mempool_tx(
                                &mempool_config, &mempool_pool, &mempool_http,
                                &push, &mut key_cache,
                            ).await {
                                tracing::error!(error = %e, txid = %push.txid, "WS mempool tx error");
                            }
                        }
                        None => {
                            tracing::warn!("[WS] Channel closed, falling back to polling only");
                            ws_receiver = None;
                        }
                    }
                }
                _ = interval.tick() => {
                    if mempool_cb.is_open() {
                        tracing::debug!("CipherScan circuit breaker open, skipping mempool scan");
                        continue;
                    }
                    match scan_mempool(&mempool_config, &mempool_pool, &mempool_http, &mempool_seen, &mut key_cache).await {
                        Ok(_) => mempool_cb.record_success(),
                        Err(e) => {
                            mempool_cb.record_failure();
                            tracing::error!(error = %e, "Mempool scan error");
                        }
                    }

                    if mempool_config.fee_enabled() {
                        let _ = billing::check_settlement_payments(&mempool_pool, &mempool_config).await;
                    }
                }
            }
        }
    });

    let block_config = config.clone();
    let block_pool = pool.clone();
    let block_http = http.clone();
    let block_seen = seen_txids.clone();
    let block_cb = circuit_breaker.clone();

    let block_handle = tokio::spawn(async move {
        let mut key_cache: Option<KeyCache> = None;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            block_config.block_poll_interval_secs,
        ));
        loop {
            interval.tick().await;
            let _ = invoices::expire_old_invoices(&block_pool).await;

            if block_cb.is_open() {
                tracing::debug!("CipherScan circuit breaker open, skipping block scan");
                continue;
            }
            match scan_blocks(
                &block_config,
                &block_pool,
                &block_http,
                &block_seen,
                &last_height,
                &mut key_cache,
            )
            .await
            {
                Ok(_) => block_cb.record_success(),
                Err(e) => {
                    block_cb.record_failure();
                    tracing::error!(error = %e, "Block scan error");
                }
            }
        }
    });

    let evict_seen = seen_txids.clone();
    let evict_handle = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SEEN_TXID_EVICT_INTERVAL));
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

    let retry_config = config.clone();
    let retry_pool = pool.clone();
    let retry_http = http.clone();
    let luma_retry_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            retry_due_luma_registrations(&retry_config, &retry_pool, &retry_http).await;
        }
    });

    let _ = tokio::join!(
        mempool_handle,
        block_handle,
        evict_handle,
        luma_retry_handle
    );
}

/// Build or refresh the PIVK cache when the merchant set changes.
/// Compares merchant IDs (not just count) so additions, deletions,
/// or replacements all trigger a rebuild.
/// When `fee_ufvk` is set, a synthetic "__platform_fee__" entry is
/// appended so settlement invoice payments to `fee_address` are
/// decrypted and matched like any other invoice.
fn refresh_key_cache<'a>(
    cache: &'a mut Option<KeyCache>,
    merchants: &[crate::merchants::Merchant],
    fee_ufvk: Option<&str>,
) -> &'a [(String, decrypt::CachedKeys)] {
    let current_ids: Vec<String> = merchants.iter().map(|m| m.id.clone()).collect();

    let needs_refresh = match cache {
        Some(c) => c.merchant_ids != current_ids,
        None => true,
    };

    if needs_refresh {
        let mut keys = Vec::with_capacity(merchants.len() + 1);
        for m in merchants {
            match decrypt::prepare_keys(&m.ufvk) {
                Ok(k) => keys.push((m.id.clone(), k)),
                Err(e) => tracing::warn!(merchant_id = %m.id, error = %e, "Failed to prepare PIVK"),
            }
        }
        if let Some(ufvk) = fee_ufvk {
            match decrypt::prepare_keys(ufvk) {
                Ok(k) => keys.push(("__platform_fee__".to_string(), k)),
                Err(e) => tracing::warn!(error = %e, "Failed to prepare fee wallet PIVK"),
            }
        }
        tracing::info!(merchants = keys.len(), "PIVK cache refreshed");
        *cache = Some(KeyCache {
            merchant_ids: current_ids,
            keys,
        });
    }

    &cache.as_ref().unwrap().keys
}

/// Fire a payment webhook without blocking the scan loop.
fn spawn_payment_webhook(
    pool: &SqlitePool,
    http: &reqwest::Client,
    invoice_id: &str,
    event: &str,
    txid: &str,
    price_zatoshis: i64,
    received_zatoshis: i64,
    overpaid: bool,
    encryption_key: &str,
) {
    let pool = pool.clone();
    let http = http.clone();
    let invoice_id = invoice_id.to_string();
    let event = event.to_string();
    let txid = txid.to_string();
    let enc_key = encryption_key.to_string();
    tokio::spawn(async move {
        if let Err(e) = webhooks::dispatch_payment(
            &pool,
            &http,
            &invoice_id,
            &event,
            &txid,
            price_zatoshis,
            received_zatoshis,
            overpaid,
            &enc_key,
        )
        .await
        {
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
    let fee_ufvk = config.fee_ufvk.as_deref();
    if merchants.is_empty() && fee_ufvk.is_none() {
        return Ok(());
    }

    let cached_keys = refresh_key_cache(key_cache, &merchants, fee_ufvk);

    let mempool_txids = mempool::fetch_mempool_txids(http, &config.cipherscan_api_url).await?;

    let new_txids: Vec<String> = {
        let seen_set = seen.read().await;
        mempool_txids
            .into_iter()
            .filter(|txid| !seen_set.contains_key(txid))
            .collect()
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
    tracing::debug!(
        fetched = raw_txs.len(),
        total = new_txids.len(),
        "Batch fetched raw txs"
    );

    let invoice_index = matching::InvoiceIndex::build(&pending);

    for (txid, raw_hex) in &raw_txs {
        let invoice_totals = invoice_detection::collect_mempool_invoice_totals(
            txid,
            raw_hex,
            cached_keys,
            &invoice_index,
            invoice_detection::MempoolSource::Polling,
        );
        invoice_detection::apply_mempool_invoice_totals(pool, http, config, txid, &invoice_totals)
            .await?;
    }

    Ok(())
}

/// Process a single mempool transaction pushed via WebSocket (with raw_hex included).
/// Skips the HTTP fetch entirely — goes straight to trial decryption.
async fn process_ws_mempool_tx(
    config: &Config,
    pool: &SqlitePool,
    http: &reqwest::Client,
    push: &ws::MempoolPush,
    key_cache: &mut Option<KeyCache>,
) -> anyhow::Result<()> {
    let pending = invoices::get_pending_invoices(pool).await?;
    if pending.is_empty() {
        return Ok(());
    }

    let merchants = crate::merchants::get_all_merchants(pool, &config.encryption_key).await?;
    let fee_ufvk = config.fee_ufvk.as_deref();
    if merchants.is_empty() && fee_ufvk.is_none() {
        return Ok(());
    }

    let cached_keys = refresh_key_cache(key_cache, &merchants, fee_ufvk);
    let invoice_index = matching::InvoiceIndex::build(&pending);

    let invoice_totals = invoice_detection::collect_mempool_invoice_totals(
        &push.txid,
        &push.raw_hex,
        cached_keys,
        &invoice_index,
        invoice_detection::MempoolSource::WebSocket,
    );
    invoice_detection::apply_mempool_invoice_totals(
        pool,
        http,
        config,
        &push.txid,
        &invoice_totals,
    )
    .await?;

    Ok(())
}

/// Max blocks to process per iteration. Keeps each call short so the
/// confirmation check at the top runs every ~block_interval seconds.
const MAX_BLOCKS_PER_SCAN: u64 = 100;

async fn scan_blocks(
    config: &Config,
    pool: &SqlitePool,
    http: &reqwest::Client,
    seen: &SeenTxids,
    last_height: &Arc<RwLock<Option<u64>>>,
    key_cache: &mut Option<KeyCache>,
) -> anyhow::Result<()> {
    let pending = invoices::get_pending_invoices(pool).await?;

    // Confirm detected invoices (uses direct txid lookup, no block scanning)
    if !pending.is_empty() {
        let detected: Vec<_> = pending
            .iter()
            .filter(|i| i.status == "detected")
            .cloned()
            .collect();
        for invoice in &detected {
            if let Some(txid) = &invoice.detected_txid {
                match blocks::check_tx_confirmed(http, &config.cipherscan_api_url, txid).await {
                    Ok(true) => {
                        let changed = invoices::mark_confirmed(pool, &invoice.id).await?;
                        if changed {
                            let overpaid =
                                invoice.received_zatoshis > invoice.price_zatoshis + 1000;
                            spawn_payment_webhook(
                                pool,
                                http,
                                &invoice.id,
                                "confirmed",
                                txid,
                                invoice.price_zatoshis,
                                invoice.received_zatoshis,
                                overpaid,
                                &config.encryption_key,
                            );
                            on_invoice_confirmed(pool, http, config, invoice).await;
                        }
                    }
                    Ok(false) => {}
                    Err(e) => tracing::debug!(txid, error = %e, "Confirmation check failed"),
                }
            }
        }
    }

    // Always track chain height so the scanner never falls behind during idle periods
    let current_height = blocks::get_chain_height(http, &config.cipherscan_api_url).await?;
    let start_height = {
        let last = last_height.read().await;
        match *last {
            Some(h) => h + 1,
            None => current_height,
        }
    };

    // Cap batch size to keep iterations short
    let batch_end = std::cmp::min(current_height, start_height + MAX_BLOCKS_PER_SCAN - 1);

    if !pending.is_empty() && start_height <= batch_end {
        if batch_end < current_height {
            tracing::info!(
                start = start_height,
                batch_end,
                chain_tip = current_height,
                behind = current_height - batch_end,
                "Block scanner catching up"
            );
        }

        let merchants = crate::merchants::get_all_merchants(pool, &config.encryption_key).await?;
        let cached_keys = refresh_key_cache(key_cache, &merchants, config.fee_ufvk.as_deref());
        let block_txids =
            blocks::fetch_block_txids(http, &config.cipherscan_api_url, start_height, batch_end)
                .await?;

        let block_invoice_index = matching::InvoiceIndex::build(&pending);

        for txid in &block_txids {
            if seen.read().await.contains_key(txid) {
                continue;
            }

            let raw_hex = match mempool::fetch_raw_tx(http, &config.cipherscan_api_url, txid).await
            {
                Ok(hex) => hex,
                Err(_) => continue,
            };

            let mut invoice_totals: HashMap<String, (invoices::Invoice, i64)> = HashMap::new();
            for (_merchant_id, keys) in cached_keys.iter() {
                if let Ok(outputs) = decrypt::try_decrypt_with_keys(&raw_hex, keys) {
                    for output in &outputs {
                        let recipient_hex = hex::encode(output.recipient_raw);
                        if let Some(invoice) =
                            block_invoice_index.find(&recipient_hex, &output.memo)
                        {
                            let entry = invoice_totals
                                .entry(invoice.id.clone())
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
                    tracing::debug!(
                        invoice_id,
                        tx_total,
                        dust_min,
                        "Ignoring dust payment in block"
                    );
                    continue;
                }

                let new_received = if invoice.status == "underpaid" {
                    invoices::accumulate_payment(pool, invoice_id, *tx_total).await?
                } else {
                    *tx_total
                };

                let min = (invoice.price_zatoshis as f64 * decrypt::SLIPPAGE_TOLERANCE) as i64;

                if new_received >= min
                    && (invoice.status == "pending" || invoice.status == "underpaid")
                {
                    let detected =
                        invoices::mark_detected(pool, invoice_id, txid, new_received).await?;
                    if detected {
                        let confirmed = invoices::mark_confirmed(pool, invoice_id).await?;
                        if confirmed {
                            let overpaid = new_received > invoice.price_zatoshis + 1000;
                            spawn_payment_webhook(
                                pool,
                                http,
                                invoice_id,
                                "confirmed",
                                txid,
                                invoice.price_zatoshis,
                                new_received,
                                overpaid,
                                &config.encryption_key,
                            );
                            on_invoice_confirmed(pool, http, config, invoice).await;
                        }
                        try_detect_fee(pool, config, &raw_hex, invoice_id).await;
                    }
                } else if new_received < min && invoice.status == "pending" {
                    invoices::mark_underpaid(pool, invoice_id, new_received, txid).await?;
                    spawn_payment_webhook(
                        pool,
                        http,
                        invoice_id,
                        "underpaid",
                        txid,
                        invoice.price_zatoshis,
                        new_received,
                        false,
                        &config.encryption_key,
                    );
                }
            }

            seen.write().await.insert(txid.clone(), Instant::now());
        }
    }

    // Always persist height progress — even when idle, keeps the scanner near chain tip
    *last_height.write().await = Some(batch_end);
    if let Err(e) = crate::db::set_scanner_state(pool, "last_height", &batch_end.to_string()).await
    {
        tracing::warn!(error = %e, "Failed to persist last_height");
    }
    Ok(())
}

/// When an invoice is confirmed, create a fee ledger entry, ensure a billing cycle exists,
/// advance the subscription period if this is a subscription invoice, and increment
/// campaign totals for donation invoices.
async fn on_invoice_confirmed(
    pool: &SqlitePool,
    http: &reqwest::Client,
    config: &Config,
    invoice: &invoices::Invoice,
) {
    // Donation campaign tracking: increment total_raised exactly once per invoice
    if invoice.is_donation == 1 && invoice.campaign_counted == 0 {
        if let Some(ref link_id) = invoice.payment_link_id {
            let amount_cents = (invoice.amount.unwrap_or(invoice.price_eur) * 100.0) as i64;
            // Atomic: set campaign_counted=1 only if still 0 (belt-and-suspenders idempotency)
            let marked = sqlx::query(
                "UPDATE invoices SET campaign_counted = 1 WHERE id = ? AND campaign_counted = 0",
            )
            .bind(&invoice.id)
            .execute(pool)
            .await;

            if let Ok(r) = marked {
                if r.rows_affected() > 0 {
                    if let Err(e) =
                        crate::payment_links::increment_raised(pool, link_id, amount_cents).await
                    {
                        tracing::error!(invoice_id = %invoice.id, error = %e, "Failed to increment campaign total_raised");
                    }
                }
            }
        }
    }
    if let Some(ref product_id) = invoice.product_id {
        match crate::events::is_product_backed_by_event(pool, product_id).await {
            Ok(true) => {
                // Check if this is a Luma-linked event
                let luma_info: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                    "SELECT e.luma_event_id, p.luma_ticket_type_id
                     FROM events e
                     LEFT JOIN prices p ON p.id = ?
                     WHERE e.product_id = ?",
                )
                .bind(invoice.price_id.as_deref())
                .bind(product_id)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);

                let luma_event_id = luma_info.as_ref().and_then(|r| r.0.as_ref());

                if let Some(luma_eid) = luma_event_id {
                    // Luma path: register guest on Luma, skip create_ticket
                    handle_luma_registration(
                        pool,
                        http,
                        config,
                        invoice,
                        luma_eid,
                        luma_info.as_ref().and_then(|r| r.1.as_deref()),
                    )
                    .await;
                } else {
                    // Private event path: create CipherPay ticket
                    match crate::tickets::create_ticket(
                        pool,
                        &invoice.id,
                        product_id,
                        invoice.price_id.as_deref(),
                        &invoice.merchant_id,
                    )
                    .await
                    {
                        Ok(Some(ticket)) => {
                            let event_ctx =
                                crate::events::get_event_context_by_product(pool, product_id)
                                    .await
                                    .ok()
                                    .flatten();
                            let payload = serde_json::json!({
                                "invoice_id": invoice.id,
                                "ticket_id": ticket.id,
                                "ticket_code": ticket.code,
                                "product_id": product_id,
                                "price_id": invoice.price_id,
                                "event_title": event_ctx.as_ref().map(|e| e.event_title.clone()),
                                "event_date": event_ctx.as_ref().and_then(|e| e.event_date.clone()),
                                "event_location": event_ctx.as_ref().and_then(|e| e.event_location.clone()),
                            });
                            let pool = pool.clone();
                            let http = http.clone();
                            let merchant_id = invoice.merchant_id.clone();
                            let enc_key = config.encryption_key.clone();
                            let inv_id = invoice.id.clone();
                            tokio::spawn(async move {
                                if let Err(e) = webhooks::dispatch_event(
                                    &pool,
                                    &http,
                                    &merchant_id,
                                    "ticket.created",
                                    payload,
                                    &enc_key,
                                )
                                .await
                                {
                                    tracing::error!(invoice_id = %inv_id, error = %e, "Failed to dispatch ticket.created webhook");
                                }
                            });
                        }
                        Ok(None) => {
                            tracing::warn!(invoice_id = %invoice.id, product_id, "Ticket creation returned None (idempotent duplicate or code collision)");
                        }
                        Err(e) => {
                            tracing::error!(invoice_id = %invoice.id, error = %e, "Failed to create ticket for confirmed invoice");
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::error!(invoice_id = %invoice.id, error = %e, "Failed to check event-backed product");
            }
        }
    }

    // Advance subscription period immediately on payment
    if let Some(ref sub_id) = invoice.subscription_id {
        match crate::subscriptions::advance_subscription_period(pool, sub_id).await {
            Ok(Some(sub)) => {
                tracing::info!(
                    sub_id,
                    invoice_id = %invoice.id,
                    new_period_end = %sub.current_period_end,
                    "Subscription advanced on payment confirmation"
                );
            }
            Ok(None) => {
                tracing::warn!(sub_id, "Subscription not found for confirmed invoice");
            }
            Err(e) => {
                tracing::error!(sub_id, error = %e, "Failed to advance subscription period");
            }
        }
    }

    if !config.fee_enabled() {
        return;
    }

    let fee_rate = match billing::get_effective_fee_rate(pool, &invoice.merchant_id, config).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get merchant fee rate, using default");
            config.fee_rate
        }
    };

    let fee_amount = invoice.price_zec * fee_rate;
    if fee_amount < 0.00025 {
        return;
    }

    if let Err(e) = billing::ensure_billing_cycle(pool, &invoice.merchant_id, config).await {
        tracing::error!(error = %e, "Failed to ensure billing cycle");
    }

    if let Err(e) = billing::create_fee_entry(
        pool,
        &invoice.id,
        &invoice.merchant_id,
        fee_amount,
        fee_rate,
    )
    .await
    {
        tracing::error!(error = %e, "Failed to create fee entry");
    }
}

/// Register a buyer on Luma after payment confirmation.
/// Decrypts stored PII, calls Luma add_guest + get_guest, stores result, then wipes PII.
async fn handle_luma_registration(
    pool: &SqlitePool,
    http: &reqwest::Client,
    config: &Config,
    invoice: &invoices::Invoice,
    luma_event_id: &str,
    luma_ticket_type_id: Option<&str>,
) {
    // Idempotency: skip if already registered
    let current_status: Option<String> =
        sqlx::query_scalar("SELECT luma_registration_status FROM invoices WHERE id = ?")
            .bind(&invoice.id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    if current_status.as_deref() == Some("registered") {
        tracing::debug!(invoice_id = %invoice.id, "Luma registration already complete, skipping");
        return;
    }

    let enc_key = &config.encryption_key;

    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT attendee_name, attendee_email FROM invoices WHERE id = ?")
            .bind(&invoice.id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);

    let (enc_name, enc_email) = match row {
        Some(r) => r,
        None => {
            tracing::error!(invoice_id = %invoice.id, "Invoice not found for Luma registration");
            return;
        }
    };

    let email = match enc_email {
        Some(ref e) if !e.is_empty() => {
            if enc_key.is_empty() {
                e.clone()
            } else {
                match crate::crypto::decrypt(e, enc_key) {
                    Ok(d) => d,
                    Err(err) => {
                        tracing::error!(invoice_id = %invoice.id, error = %err, "Failed to decrypt attendee email");
                        mark_luma_failed(pool, &invoice.id).await;
                        return;
                    }
                }
            }
        }
        _ => {
            tracing::error!(invoice_id = %invoice.id, "No attendee email for Luma registration");
            mark_luma_failed(pool, &invoice.id).await;
            return;
        }
    };

    let name = enc_name
        .as_ref()
        .filter(|n| !n.is_empty())
        .map(|n| {
            if enc_key.is_empty() {
                Ok(n.clone())
            } else {
                crate::crypto::decrypt(n, enc_key)
            }
        })
        .transpose()
        .ok()
        .flatten();

    // Decrypt merchant's Luma API key
    let api_key: Option<String> =
        sqlx::query_scalar("SELECT luma_api_key FROM merchants WHERE id = ?")
            .bind(&invoice.merchant_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();

    let api_key = match api_key {
        Some(k) if !k.is_empty() => {
            if enc_key.is_empty() {
                k
            } else {
                match crate::crypto::decrypt(&k, enc_key) {
                    Ok(d) => d,
                    Err(err) => {
                        tracing::error!(invoice_id = %invoice.id, error = %err, "Failed to decrypt Luma API key");
                        mark_luma_failed(pool, &invoice.id).await;
                        return;
                    }
                }
            }
        }
        _ => {
            tracing::error!(invoice_id = %invoice.id, "No Luma API key for merchant");
            mark_luma_failed(pool, &invoice.id).await;
            return;
        }
    };

    // Call Luma add_guest
    match crate::luma::add_guest(
        http,
        &api_key,
        luma_event_id,
        &email,
        name.as_deref(),
        luma_ticket_type_id,
    )
    .await
    {
        Ok(resp) => {
            tracing::info!(
                invoice_id = %invoice.id,
                luma_event_id,
                approval_status = ?resp.approval_status,
                "Luma guest added"
            );
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::error!(invoice_id = %invoice.id, error = %err_str, "Failed to add guest on Luma");
            if is_transient_luma_error(&err_str) {
                schedule_luma_retry(pool, &invoice.id).await;
            } else {
                mark_luma_failed(pool, &invoice.id).await;
            }
            return;
        }
    }

    // Call Luma get_guest to retrieve check-in QR and full guest record
    let guest_data = match crate::luma::get_guest(http, &api_key, luma_event_id, &email).await {
        Ok(Some(g)) => serde_json::to_string(&g).unwrap_or_else(|_| "{}".into()),
        Ok(None) => {
            tracing::warn!(invoice_id = %invoice.id, "Luma get_guest returned empty after add");
            "{}".into()
        }
        Err(e) => {
            tracing::warn!(invoice_id = %invoice.id, error = %e, "Failed to get guest details from Luma (registration still succeeded)");
            "{}".into()
        }
    };

    // Store result and delete PII
    sqlx::query(
        "UPDATE invoices SET luma_registration_status = 'registered', luma_guest_data = ?, attendee_name = NULL, attendee_email = NULL, luma_retry_at = NULL WHERE id = ?",
    )
    .bind(&guest_data)
    .bind(&invoice.id)
    .execute(pool)
    .await
    .ok();

    tracing::info!(invoice_id = %invoice.id, "Luma registration complete, PII deleted");

    // Dispatch webhook
    let payload = serde_json::json!({
        "invoice_id": invoice.id,
        "product_id": invoice.product_id,
        "luma_event_id": luma_event_id,
        "luma_registered": true,
    });
    let pool = pool.clone();
    let http = http.clone();
    let merchant_id = invoice.merchant_id.clone();
    let enc_key = config.encryption_key.clone();
    let inv_id = invoice.id.clone();
    tokio::spawn(async move {
        if let Err(e) = webhooks::dispatch_event(
            &pool,
            &http,
            &merchant_id,
            "luma.registered",
            payload,
            &enc_key,
        )
        .await
        {
            tracing::error!(invoice_id = %inv_id, error = %e, "Failed to dispatch luma.registered webhook");
        }
    });
}

fn is_transient_luma_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection")
        || lower.contains("429")
        || lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
}

const MAX_LUMA_RETRIES: i64 = 5;

async fn schedule_luma_retry(pool: &SqlitePool, invoice_id: &str) {
    let retry_count: i64 =
        sqlx::query_scalar("SELECT COALESCE(luma_retry_count, 0) FROM invoices WHERE id = ?")
            .bind(invoice_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    let new_count = retry_count + 1;
    if new_count > MAX_LUMA_RETRIES {
        tracing::error!(
            invoice_id,
            retries = new_count,
            "Luma registration exceeded max retries, marking failed"
        );
        mark_luma_failed(pool, invoice_id).await;
        return;
    }

    // Exponential backoff: min(60, 5 * 2^(attempt-1)) minutes
    let backoff_minutes = std::cmp::min(60, 5 * (1i64 << (new_count - 1)));
    let retry_at = chrono::Utc::now() + chrono::Duration::minutes(backoff_minutes);
    let retry_at_str = retry_at.format("%Y-%m-%dT%H:%M:%S").to_string();

    tracing::warn!(
        invoice_id,
        attempt = new_count,
        retry_at = %retry_at_str,
        "Scheduling Luma registration retry"
    );

    sqlx::query(
        "UPDATE invoices SET luma_registration_status = 'retry', luma_retry_count = ?, luma_retry_at = ? WHERE id = ?"
    )
    .bind(new_count)
    .bind(&retry_at_str)
    .bind(invoice_id)
    .execute(pool)
    .await
    .ok();
}

async fn retry_due_luma_registrations(config: &Config, pool: &SqlitePool, http: &reqwest::Client) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT i.id, i.product_id, i.price_id FROM invoices i
         WHERE i.luma_registration_status = 'retry'
         AND i.luma_retry_at IS NOT NULL
         AND i.luma_retry_at <= ?
         AND i.status IN ('detected', 'confirmed')",
    )
    .bind(&now)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    if rows.is_empty() {
        return;
    }

    tracing::info!(
        count = rows.len(),
        "Processing due Luma registration retries"
    );

    for (invoice_id, product_id, price_id) in rows {
        let product_id = match product_id {
            Some(pid) => pid,
            None => continue,
        };

        let invoice = match invoices::get_invoice(pool, &invoice_id).await {
            Ok(Some(inv)) => inv,
            _ => continue,
        };

        let luma_info: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT e.luma_event_id, p.luma_ticket_type_id
             FROM events e
             LEFT JOIN prices p ON p.id = ?
             WHERE e.product_id = ?",
        )
        .bind(price_id.as_deref())
        .bind(&product_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);

        let luma_event_id = luma_info.as_ref().and_then(|r| r.0.as_ref());
        if let Some(luma_eid) = luma_event_id {
            tracing::info!(invoice_id = %invoice.id, attempt = "retry", "Retrying Luma registration");
            handle_luma_registration(
                pool,
                http,
                config,
                &invoice,
                luma_eid,
                luma_info.as_ref().and_then(|r| r.1.as_deref()),
            )
            .await;
        }
    }
}

async fn mark_luma_failed(pool: &SqlitePool, invoice_id: &str) {
    sqlx::query("UPDATE invoices SET luma_registration_status = 'failed', luma_retry_at = NULL WHERE id = ?")
        .bind(invoice_id)
        .execute(pool)
        .await
        .ok();
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
