use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::invoices;
use crate::invoices::matching;

#[derive(Clone, Copy)]
pub(super) enum MempoolSource {
    Polling,
    WebSocket,
}

type InvoiceTotals = HashMap<String, (invoices::Invoice, i64)>;
pub(super) type CampaignTotals = HashMap<String, (crate::payment_links::CampaignAddress, i64)>;

pub(super) fn collect_mempool_invoice_totals(
    txid: &str,
    raw_hex: &str,
    cached_keys: &[(String, super::decrypt::CachedKeys)],
    invoice_index: &matching::InvoiceIndex<'_>,
    campaign_addresses: &[crate::payment_links::CampaignAddress],
    source: MempoolSource,
) -> (InvoiceTotals, CampaignTotals) {
    let mut invoice_totals = HashMap::new();
    let mut campaign_totals: CampaignTotals = HashMap::new();

    for (_merchant_id, keys) in cached_keys {
        match super::decrypt::try_decrypt_with_keys(raw_hex, keys) {
            Ok(outputs) => {
                for output in &outputs {
                    let recipient_hex = hex::encode(output.recipient_raw);
                    match source {
                        MempoolSource::Polling => tracing::debug!(txid, "Decrypted mempool output"),
                        MempoolSource::WebSocket => {
                            tracing::debug!(txid = %txid, "[WS] Decrypted mempool output");
                        }
                    }
                    tracing::debug!(
                        txid,
                        memo = %output.memo,
                        amount = output.amount_zec,
                        "Decrypted output details"
                    );

                    if let Some(invoice) = invoice_index.find(&recipient_hex, &output.memo) {
                        let entry = invoice_totals
                            .entry(invoice.id.clone())
                            .or_insert((invoice.clone(), 0));
                        entry.1 += output.amount_zatoshis as i64;
                    } else if let Some(campaign) = campaign_addresses.iter().find(|c| c.campaign_address_hex == recipient_hex) {
                        let entry = campaign_totals
                            .entry(campaign.link_id.clone())
                            .or_insert((campaign.clone(), 0));
                        entry.1 += output.amount_zatoshis as i64;
                    }
                }
            }
            Err(_) => {}
        }
    }

    (invoice_totals, campaign_totals)
}

/// Returns list of invoice IDs that were newly marked as detected (for fee scanning).
pub(super) async fn apply_mempool_invoice_totals(
    pool: &SqlitePool,
    http: &reqwest::Client,
    config: &Config,
    txid: &str,
    invoice_totals: &InvoiceTotals,
) -> anyhow::Result<Vec<String>> {
    let mut newly_detected = Vec::new();

    for (invoice_id, (invoice, tx_total)) in invoice_totals {
        let dust_min = std::cmp::max(
            (invoice.price_zatoshis as f64 * super::decrypt::DUST_THRESHOLD_FRACTION) as i64,
            super::decrypt::DUST_THRESHOLD_MIN_ZATOSHIS,
        );
        if *tx_total < dust_min && *tx_total < invoice.price_zatoshis {
            tracing::debug!(invoice_id, tx_total, dust_min, "Ignoring dust payment");
            continue;
        }

        let new_received = if invoice.status == "underpaid" {
            invoices::record_payment(pool, invoice_id, txid, *tx_total).await?
        } else {
            *tx_total
        };

        let min = (invoice.price_zatoshis as f64 * super::decrypt::SLIPPAGE_TOLERANCE) as i64;

        if new_received >= min {
            let changed = invoices::mark_detected(pool, invoice_id, txid, new_received).await?;
            if changed {
                super::metrics::global().record_payment_detected();
                newly_detected.push(invoice_id.clone());
                let overpaid = new_received > invoice.price_zatoshis + 1000;
                super::spawn_payment_webhook(
                    pool,
                    http,
                    invoice_id,
                    "detected",
                    txid,
                    invoice.price_zatoshis,
                    new_received,
                    overpaid,
                    &config.encryption_key,
                );
            }
        } else if invoice.status == "pending" {
            invoices::mark_underpaid(pool, invoice_id, new_received, txid).await?;
            super::spawn_payment_webhook(
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

    Ok(newly_detected)
}

/// Attribute direct (off-invoice) donations to campaign catch-all addresses.
/// Uses INSERT OR IGNORE for idempotency — safe from both mempool and block paths.
pub(super) async fn apply_campaign_totals(
    pool: &SqlitePool,
    txid: &str,
    campaign_totals: &CampaignTotals,
    price_service: &crate::invoices::pricing::PriceService,
) {
    for (link_id, (campaign, zat_total)) in campaign_totals {
        if *zat_total <= 0 {
            continue;
        }
        let zec_amount = *zat_total as f64 / 100_000_000.0;
        let rates = price_service.get_rates().await;
        let cents = match rates {
            Ok(r) => {
                let rate = match campaign.currency.as_str() {
                    "USD" => r.zec_usd,
                    "GBP" => r.zec_gbp,
                    "CAD" => r.zec_cad,
                    "BRL" => r.zec_brl,
                    _ => r.zec_eur,
                };
                (zec_amount * rate * 100.0) as i64
            }
            Err(_) => {
                tracing::warn!(link_id, "No price data for direct donation, skipping");
                continue;
            }
        };
        match crate::payment_links::record_campaign_donation(
            pool, link_id, txid, *zat_total, cents, &campaign.currency,
        ).await {
            Ok(true) => {
                tracing::info!(
                    link_id,
                    txid,
                    zatoshis = zat_total,
                    fiat_cents = cents,
                    currency = %campaign.currency,
                    "Direct donation attributed to campaign"
                );
            }
            Ok(false) => {}
            Err(e) => {
                tracing::error!(link_id, error = %e, "Failed to record direct campaign donation");
            }
        }
    }
}
