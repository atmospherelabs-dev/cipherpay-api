//! Source-token fee accounting, separate from the merchant's ZEC billing ledger.
//! Public receipts establish actual collection. Confidential fees can instead
//! be accepted as provider-handled after our exact swap and ZEC receipt settle;
//! that policy never invents received token units or referral earnings.
use super::*;

const TOTAL_BPS: u64 = 250;
const CIPHERPAY_BPS: u64 = 125;
const CONFIDENTIAL_PROVIDER_BPS: u64 = 20;
const RPC: &str = "https://archival-rpc.mainnet.near.org";

pub(super) fn apply(body: &mut Value, recipient: Option<&str>) {
    if let Some(recipient) = recipient {
        let fee = if body["confidentiality"] == CONFIDENTIALITY { CIPHERPAY_BPS } else { TOTAL_BPS };
        body["appFees"] = json!([{ "recipient": recipient, "fee": fee }]);
    }
}

// Confidential quotes retain our requested share and add 20 bps for 1Click.
// Legacy public quotes split the requested combined fee evenly.
fn terms(body: &Value) -> anyhow::Result<(u64, u64)> {
    let requested = body["appFees"][0]["fee"].as_u64().context("Missing requested fee")?;
    if body["confidentiality"] == CONFIDENTIALITY {
        Ok((requested.checked_add(CONFIDENTIAL_PROVIDER_BPS).context("Fee overflow")?, requested))
    } else {
        Ok((requested, requested / 2))
    }
}

pub(super) fn validate_quote(response: &Value, body: &Value) -> anyhow::Result<()> {
    let Some(requested) = body.get("appFees") else {
        return Ok(());
    };
    let recipient = requested[0]["recipient"]
        .as_str()
        .context("Missing fee recipient")?;
    let fees = response["quoteRequest"]["appFees"]
        .as_array()
        .context("Missing fee allocation")?;
    let mut total = 0_u64;
    let mut ours = 0_u64;
    for fee in fees {
        let bps = fee["fee"].as_u64().context("Invalid fee allocation")?;
        total = total.checked_add(bps).context("Fee overflow")?;
        ensure!(
            fee["recipient"].as_str().is_some_and(|s| !s.is_empty()),
            "Missing fee recipient"
        );
        if fee["recipient"] == recipient {
            ours = ours.checked_add(bps).context("Fee overflow")?;
        }
    }
    let (expected_total, expected_ours) = terms(body)?;
    // Reject changes to buyer costs or the allocation of our revenue.
    ensure!(
        total == expected_total && ours == expected_ours,
        "Unexpected provider fee split"
    );
    Ok(())
}

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE crosschain_fees (
        invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
        asset_id TEXT NOT NULL, recipient TEXT NOT NULL,
        total_bps INTEGER NOT NULL, cipherpay_bps INTEGER NOT NULL,
        state TEXT NOT NULL DEFAULT 'reserved', correlation_id TEXT UNIQUE,
        quoted_input TEXT, received_units TEXT, evidence TEXT,
        next_check INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0,
        reconciled_at TEXT
    )",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE INDEX crosschain_fees_pending ON crosschain_fees(next_check) WHERE state != 'collected'")
        .execute(&mut *tx).await?;
    sqlx::query("CREATE TABLE crosschain_fee_evidence (
        proof_id TEXT PRIMARY KEY, invoice_id TEXT NOT NULL REFERENCES crosschain_fees(invoice_id) ON DELETE CASCADE
    )").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn reserve(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    body: &Value,
) -> anyhow::Result<()> {
    let Some(fees) = body.get("appFees") else {
        return Ok(());
    };
    let billed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fee_ledger WHERE invoice_id=?)")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    ensure!(!billed, "Invoice already has merchant fee accounting");
    let (total, ours) = terms(body)?;
    sqlx::query("INSERT INTO crosschain_fees (invoice_id,asset_id,recipient,total_bps,cipherpay_bps) VALUES (?,?,?,?,?)")
        .bind(id).bind(body["originAsset"].as_str().context("Missing asset")?)
        .bind(fees[0]["recipient"].as_str().context("Missing fee recipient")?)
        .bind(i64::try_from(total)?).bind(i64::try_from(ours)?).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn bind_quote(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    response: &Value,
    body: &Value,
) -> anyhow::Result<()> {
    if body.get("appFees").is_none() {
        return Ok(());
    }
    let correlation = response["correlationId"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Missing quote correlation")?;
    sqlx::query("UPDATE crosschain_fees SET correlation_id=?,quoted_input=?,state='pending' WHERE invoice_id=? AND state='reserved'")
        .bind(correlation).bind(response["quote"]["amountIn"].as_str().context("Missing input")?)
        .bind(id).execute(&mut **tx).await?;
    Ok(())
}

#[derive(Debug)]
struct Credit {
    proof_id: String,
    units: u128,
}

fn credits(
    result: &Value,
    tx_hash: &str,
    intents: &[String],
    recipient: &str,
    asset: &str,
) -> anyhow::Result<Vec<Credit>> {
    ensure!(
        result["final_execution_status"] == "FINAL",
        "NEAR transaction is not final"
    );
    ensure!(
        result["transaction"]["hash"] == tx_hash,
        "NEAR transaction mismatch"
    );
    ensure!(
        result["status"].get("SuccessValue").is_some(),
        "NEAR transaction did not succeed"
    );
    let outcomes = result["receipts_outcome"]
        .as_array()
        .context("Missing NEAR receipts")?;
    let mut found = Vec::new();
    for receipt in outcomes {
        let outcome = &receipt["outcome"];
        if outcome["executor_id"] != "intents.near"
            || outcome["status"].get("SuccessValue").is_none()
        {
            continue;
        }
        let receipt_id = receipt["id"].as_str().context("Missing receipt id")?;
        for (log_index, log) in outcome["logs"]
            .as_array()
            .context("Missing logs")?
            .iter()
            .enumerate()
        {
            let Some(raw) = log.as_str().and_then(|s| s.strip_prefix("EVENT_JSON:")) else {
                continue;
            };
            let event: Value = serde_json::from_str(raw)?;
            if event["standard"] != "dip4" || event["event"] != "transfer" {
                continue;
            }
            for (index, transfer) in event["data"]
                .as_array()
                .context("Invalid transfer events")?
                .iter()
                .enumerate()
            {
                if transfer["receiver_id"] != recipient {
                    continue;
                }
                let Some(intent) = transfer["intent_hash"].as_str() else {
                    continue;
                };
                if !intents.iter().any(|h| h == intent) {
                    continue;
                }
                let Some(amount) = transfer["tokens"].get(asset) else {
                    continue;
                };
                found.push(Credit {
                    proof_id: format!("{receipt_id}:{log_index}:{index}:{asset}"),
                    units: positive_units(amount)?,
                });
            }
        }
    }
    Ok(found)
}

async fn record(
    pool: &SqlitePool,
    id: &str,
    credits: &[Credit],
    evidence: &str,
) -> anyhow::Result<()> {
    ensure!(!credits.is_empty(), "No attributable fee receipt yet");
    let amount = credits.iter().try_fold(0_u128, |sum, c| {
        sum.checked_add(c.units).context("Fee amount overflow")
    })?;
    let mut tx = pool.begin().await?;
    for c in credits {
        sqlx::query("INSERT INTO crosschain_fee_evidence (proof_id,invoice_id) VALUES (?,?) ON CONFLICT(proof_id) DO NOTHING")
            .bind(&c.proof_id).bind(id).execute(&mut *tx).await?;
        let owner: String =
            sqlx::query_scalar("SELECT invoice_id FROM crosschain_fee_evidence WHERE proof_id=?")
                .bind(&c.proof_id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            owner == id,
            "Fee receipt already belongs to another invoice"
        );
    }
    sqlx::query("UPDATE crosschain_fees SET state='collected',received_units=?,evidence=?,reconciled_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE invoice_id=? AND state != 'collected'")
        .bind(amount.to_string()).bind(evidence).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// Status responses omit creation correlationId. Bind the immutable route and
// deposit instead; an optional correlation, if returned, must still match.
fn validate_settlement(
    data: &Value,
    q: &Value,
    recipient: &str,
    correlation: Option<&str>,
) -> anyhow::Result<()> {
    ensure!(data["status"] == "SUCCESS", "Swap is not settled");
    validate_route(data, q, recipient, correlation)
}

pub(super) fn validate_route(
    data: &Value,
    q: &Value,
    recipient: &str,
    correlation: Option<&str>,
) -> anyhow::Result<()> {
    let response = &data["quoteResponse"];
    validate_confidentiality(response, q)?;
    if let Some(actual) = response["correlationId"].as_str() {
        ensure!(Some(actual) == correlation, "Swap correlation mismatch");
    }
    for field in ["depositAddress", "depositMemo"] {
        ensure!(response["quote"][field] == q[field], "Deposit mismatch");
    }
    ensure!(
        q["depositAddress"].as_str().is_some_and(|s| !s.is_empty()),
        "Missing deposit"
    );
    let expected = json!({"originAsset":q["asset"]["assetId"],"destinationAsset":ZEC,
        "recipient":recipient,"refundTo":q["refundAddress"],"amount":q["amountOut"],"swapType":"EXACT_OUTPUT"});
    for field in [
        "originAsset",
        "destinationAsset",
        "recipient",
        "refundTo",
        "amount",
        "swapType",
    ] {
        ensure!(
            response["quoteRequest"][field] == expected[field],
            "Settlement route mismatch"
        );
    }
    ensure!(
        response["quote"]["amountOut"] == q["amountOut"],
        "Settlement output mismatch"
    );
    Ok(())
}

pub async fn migrate_settlements(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE crosschain_settlements (
        invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
        verified_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
    )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn migrate_provider_handled(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("CREATE TABLE crosschain_provider_confirmations (
        invoice_id TEXT PRIMARY KEY REFERENCES crosschain_fees(invoice_id) ON DELETE CASCADE,
        confirmation TEXT NOT NULL,
        confirmed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
    )").execute(pool).await?;
    Ok(())
}

fn zcash_deliveries(details: &Value) -> anyhow::Result<Vec<String>> {
    let entries = details["destinationChainTxHashes"]
        .as_array()
        .context("Missing ZEC delivery")?;
    ensure!(!entries.is_empty() && entries.len() <= 16, "Invalid delivery count");
    let mut hashes = Vec::new();
    for entry in entries {
        let hash = entry["hash"].as_str().context("Missing delivery hash")?;
        ensure!(!hash.is_empty() && hash.len() <= 128, "Invalid delivery hash");
        // 1Click can include an origin-chain refund here alongside the ZEC
        // withdrawal. Only Zcash-shaped hashes are scanner candidates. A hash
        // alone is never proof: settlement still requires this invoice's actual
        // shielded outputs to cover its price.
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            let hash = hash.to_ascii_lowercase();
            if !hashes.contains(&hash) {
                hashes.push(hash);
            }
        }
    }
    ensure!(!hashes.is_empty(), "Missing ZEC delivery");
    Ok(hashes)
}

async fn matched_delivery(pool: &SqlitePool, g: &Gateway, id: &str, hashes: &[String]) -> anyhow::Result<i64> {
    let mut delivered = 0_i64;
    for hash in hashes {
        let units: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(zatoshis),0) FROM invoice_payments WHERE invoice_id=? AND lower(txid)=?")
            .bind(id).bind(hash).fetch_one(pool).await?;
        // invoice_payments also contains mempool observations. Another transfer
        // may confirm the invoice while this route's output remains unconfirmed
        // or reorged, so check each original delivery's canonical inclusion.
        if units > 0 && crate::scanner::blocks::check_tx_confirmed(
            &g.confirmation_client, &g.cipherscan_api_url, hash,
        ).await? {
            delivered = delivered.checked_add(units).context("Delivery overflow")?;
        }
    }
    Ok(delivered)
}

async fn reconcile(pool: &SqlitePool, g: &Gateway, id: &str, key: &str) -> anyhow::Result<()> {
    let terminal: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM crosschain_fees WHERE invoice_id=? AND state IN ('collected','direct','provider_handled'))")
        .bind(id).fetch_one(pool).await?;
    if terminal {
        return Ok(());
    }
    let attempt = read_attempt(pool, id, key)
        .await?
        .context("Missing payment attempt")?;
    if attempt["quote"].is_null() {
        let confirmed: bool =
            sqlx::query_scalar("SELECT status='confirmed' FROM invoices WHERE id=?")
                .bind(id)
                .fetch_one(pool)
                .await?;
        if confirmed {
            resolve_direct(pool, id).await?;
            return Ok(());
        }
    }
    let q = &attempt["quote"];
    let deposit = q["depositAddress"].as_str().context("Quote needs review")?;
    let mut req = g
        .request(reqwest::Method::GET, "/status")
        .query(&[("depositAddress", deposit)]);
    if let Some(memo) = q["depositMemo"].as_str().filter(|s| !s.is_empty()) {
        req = req.query(&[("depositMemo", memo)]);
    }
    let data: Value = req.send().await?.error_for_status()?.json().await?;
    reconcile_status(pool, g, id, key, &attempt, &data).await
}

async fn reconcile_status(
    pool: &SqlitePool,
    g: &Gateway,
    id: &str,
    key: &str,
    attempt: &Value,
    data: &Value,
) -> anyhow::Result<()> {
    let q = &attempt["quote"];
    let fee: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT asset_id,recipient,correlation_id FROM crosschain_fees WHERE invoice_id=?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let inv = crate::invoices::get_invoice(pool, id)
        .await?
        .context("Missing invoice")?;
    let correlation = fee.as_ref().and_then(|f| f.2.as_deref());
    signatures::verify_status(&data["quoteResponse"])?;
    validate_route(&data, q, &inv.payment_address, correlation)?;
    // An expired, still-unfunded route (or a completed refund) cannot account
    // for an already confirmed ZEC transfer. Resolve the retained direct fee.
    let deadline_passed = q["providerDeadline"]
        .as_str()
        .or(q["deadline"].as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|d| d < Utc::now());
    if inv.status == "confirmed"
        && (data["status"] == "REFUNDED"
            || (data["status"] == "PENDING_DEPOSIT" && deadline_passed))
    {
        resolve_direct(pool, id).await?;
        return Ok(());
    }
    validate_settlement(&data, q, &inv.payment_address, correlation)?;
    ensure!(
        positive_units(&q["amountOut"])? == inv.price_zatoshis as u128,
        "Invoice amount mismatch"
    );
    if let Some((asset, recipient, _)) = &fee {
        ensure!(correlation.is_some(), "Unbound quote");
        ensure!(q["asset"]["assetId"] == *asset, "Fee asset mismatch");
        let mut body = json!({});
        if let Some(mode) = q.get("confidentiality") {
            body["confidentiality"] = mode.clone();
        }
        apply(&mut body, Some(recipient));
        let (total, ours): (i64, i64) =
            sqlx::query_as("SELECT total_bps,cipherpay_bps FROM crosschain_fees WHERE invoice_id=?")
                .bind(id)
                .fetch_one(pool)
                .await?;
        body["appFees"][0]["fee"] = json!(if q["confidentiality"] == CONFIDENTIALITY { ours } else { total });
        validate_quote(&data["quoteResponse"], &body)?;
    }
    let details = &data["swapDetails"];
    let destination = zcash_deliveries(details)?;
    // Queue exact destination txids for the scanner. Provider SUCCESS alone
    // never credits the invoice; raw ZEC outputs and finality remain authoritative.
    let mut recovery = pool.begin().await?;
    for hash in &destination {
        sqlx::query("INSERT OR IGNORE INTO crosschain_deliveries(invoice_id,txid) VALUES (?,?)")
            .bind(id)
            .bind(hash)
            .execute(&mut *recovery)
            .await?;
    }
    sqlx::query(
        "UPDATE invoices SET status=CASE WHEN status='expired' THEN 'pending' ELSE status END,
        expires_at=MAX(expires_at,strftime('%Y-%m-%dT%H:%M:%SZ','now','+24 hours'))
        WHERE id=? AND status IN ('pending','underpaid','expired')",
    )
    .bind(id)
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let delivered = matched_delivery(pool, g, id, &destination).await?;
    ensure!(
        delivered >= inv.price_zatoshis,
        "Full ZEC delivery has not been matched by our scanner"
    );
    let provider_handled = fee.is_some() && q["confidentiality"] == CONFIDENTIALITY;
    if provider_handled {
        ensure!(inv.status == "confirmed", "ZEC payment is not confirmed");
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if provider_handled {
        let status: String = sqlx::query_scalar("SELECT status FROM invoices WHERE id=?")
            .bind(id).fetch_one(&mut *tx).await?;
        ensure!(status == "confirmed", "ZEC payment is not confirmed");
        let state: String = sqlx::query_scalar("SELECT state FROM crosschain_fees WHERE invoice_id=?")
            .bind(id).fetch_one(&mut *tx).await?;
        if matches!(state.as_str(), "collected" | "direct" | "provider_handled") {
            return Ok(());
        }
    }
    sqlx::query("INSERT OR IGNORE INTO crosschain_settlements(invoice_id) VALUES (?)")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE crosschain_attempts SET state='SUCCESS',last_success=? WHERE invoice_id=?")
        .bind(Utc::now().timestamp())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE deferred_payment_fees SET state='crosschain' WHERE invoice_id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if provider_handled {
        // This is the explicitly accepted provider-trust policy, separate from
        // independent credit evidence. Keep source-token cash amounts absent.
        let confirmation = crate::crypto::encrypt(&json!({
            "policy":"provider_handled", "provider":"near_1click", "status":data,
            "correlation_id":correlation,
        }).to_string(), key)?;
        let updated = sqlx::query("UPDATE crosschain_fees SET state='provider_handled',failures=0,next_check=0,reconciled_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE invoice_id=? AND state NOT IN ('collected','direct','provider_handled')")
            .bind(id).execute(&mut *tx).await?;
        if updated.rows_affected() == 1 {
            sqlx::query("INSERT INTO crosschain_provider_confirmations(invoice_id,confirmation) VALUES (?,?)")
                .bind(id).bind(confirmation).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        return Ok(());
    }
    tx.commit().await?;
    // Legacy fee-free payments still need a verified payment-method record.
    let Some((asset, recipient, _)) = fee else {
        return Ok(());
    };
    let intents: Vec<String> = serde_json::from_value(details["intentHashes"].clone())?;
    let hashes: Vec<String> = serde_json::from_value(details["nearTxHashes"].clone())?;
    ensure!(
        !intents.is_empty() && intents.len() <= 64 && !hashes.is_empty() && hashes.len() <= 16,
        "Invalid settlement references"
    );
    let mut all = Vec::new();
    for hash in &hashes {
        let response: Value = g
            .client
            .post(RPC)
            .json(&json!({"jsonrpc":"2.0","id":"fee","method":"tx",
            "params":{"tx_hash":hash,"sender_account_id":"intents.near","wait_until":"FINAL"}}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        all.extend(credits(
            &response["result"],
            hash,
            &intents,
            &recipient,
            &asset,
        )?);
    }
    all.sort_by(|a, b| a.proof_id.cmp(&b.proof_id));
    all.dedup_by(|a, b| a.proof_id == b.proof_id);
    // Store actual credit events, not a percentage-derived estimate. Evidence
    // connects the public-chain payment to a shielded invoice, so encrypt it.
    let evidence = crate::crypto::encrypt(&json!({"nearTxHashes":hashes,"intentHashes":intents,
        "credits":all.iter().map(|c|json!({"proof":c.proof_id,"units":c.units.to_string()})).collect::<Vec<_>>()}).to_string(),key)?;
    for credit in &mut all {
        credit.proof_id = crate::crypto::blind_index(&hex::encode(credit.proof_id.as_bytes()), key);
    }
    record(pool, id, &all, &evidence).await
}

/// Merchant-scoped actual receipts. Never mix units from different tokens or
/// count an unpaid quote as a pending bill. No deposit/refund addresses exposed.
pub async fn merchant_summary(
    pool: &SqlitePool,
    merchant: &str,
    key: &str,
    enabled: bool,
) -> anyhow::Result<Value> {
    let rows: Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    )> = sqlx::query_as(
        "SELECT i.id,i.memo_code,f.state,f.received_units,a.payload,i.created_at
         FROM crosschain_fees f JOIN invoices i ON i.id=f.invoice_id
         JOIN crosschain_attempts a ON a.invoice_id=i.id
         WHERE i.merchant_id=? AND (i.status IN ('detected','confirmed') OR f.state='collected')
         ORDER BY i.created_at DESC",
    )
    .bind(merchant)
    .fetch_all(pool)
    .await?;
    let mut assets: std::collections::BTreeMap<String, (Asset, u128, usize)> =
        std::collections::BTreeMap::new();
    let mut recent = Vec::new();
    let mut collected = 0;
    let mut pending = 0;
    let mut provider_handled = 0;
    for (id, reference, state, received, payload, created) in rows {
        if state == "direct" {
            continue;
        }
        let Some(payload) = payload else {
            pending += 1;
            continue;
        };
        let q: Value = serde_json::from_str(&crate::crypto::decrypt(&payload, key)?)?;
        let asset: Asset = serde_json::from_value(q["asset"].clone())?;
        if state == "collected" {
            let amount = received
                .as_ref()
                .context("Missing received fee")?
                .parse::<u128>()?;
            let entry = assets
                .entry(asset.asset_id.clone())
                .or_insert((asset.clone(), 0, 0));
            entry.1 = entry.1.checked_add(amount).context("Fee total overflow")?;
            entry.2 += 1;
            collected += 1;
        } else if state == "provider_handled" {
            provider_handled += 1;
        } else {
            pending += 1;
        }
        if recent.len() < 10 {
            recent.push(json!({"invoice_id":id,"reference":reference,"state":if state == "collected" {"collected"} else if state == "provider_handled" {"provider_handled"} else {"pending"},
                "received_units":received,"asset":asset,"created_at":created}));
        }
    }
    let service: i64=sqlx::query_scalar("SELECT MIN(1000,MAX(0,CAST(ROUND(COALESCE(fee_rate,0.01)*10000) AS INTEGER)), CASE WHEN EXISTS(SELECT 1 FROM merchant_referrals r WHERE r.merchant_id=merchants.id AND r.discount_until>strftime('%Y-%m-%dT%H:%M:%SZ','now')) THEN 50 ELSE 1000 END) FROM merchants WHERE id=?")
        .bind(merchant).fetch_one(pool).await?;
    Ok(
        json!({"enabled":enabled,"total_bps":service+25+CONFIDENTIAL_PROVIDER_BPS as i64,"cipherpay_bps":service+25,
        "collected_count":collected,"provider_handled_count":provider_handled,"pending_count":pending,"recent":recent,
        "assets":assets.into_values().map(|(asset, amount, count)|json!({"asset":asset,"received_units":amount.to_string(),"count":count})).collect::<Vec<_>>() }),
    )
}

pub async fn payment_methods(pool: &SqlitePool, merchant: &str) -> anyhow::Result<Value> {
    let (direct, crosschain, verifying): (i64, i64, i64) = sqlx::query_as(
        "SELECT
          COUNT(CASE WHEN s.invoice_id IS NULL AND (a.invoice_id IS NULL OR f.state='direct') THEN 1 END),
          COUNT(CASE WHEN s.invoice_id IS NOT NULL THEN 1 END),
          COUNT(CASE WHEN a.invoice_id IS NOT NULL AND s.invoice_id IS NULL AND COALESCE(f.state,'')!='direct' THEN 1 END)
         FROM invoices i LEFT JOIN crosschain_attempts a ON a.invoice_id=i.id
         LEFT JOIN crosschain_settlements s ON s.invoice_id=i.id
         LEFT JOIN crosschain_fees f ON f.invoice_id=i.id
         WHERE i.merchant_id=? AND i.status='confirmed'",
    )
    .bind(merchant)
    .fetch_one(pool)
    .await?;
    Ok(json!({"direct_zec":direct,"crosschain":crosschain,"verifying":verifying}))
}

pub(super) async fn resolve_direct(pool: &SqlitePool, id: &str) -> anyhow::Result<()> {
    let obligation: Option<(String,f64,f64)> = sqlx::query_as("UPDATE deferred_payment_fees SET state='direct' WHERE invoice_id=? AND state!='crosschain' RETURNING merchant_id,amount_zec,rate")
        .bind(id).fetch_optional(pool).await?;
    if let Some((merchant, amount, rate)) = obligation {
        crate::billing::create_fee_entry(pool, id, &merchant, amount, rate).await?;
        let receipts: Vec<(String, i64)> =
            sqlx::query_as("SELECT txid,units FROM direct_fee_receipts WHERE invoice_id=?")
                .bind(id)
                .fetch_all(pool)
                .await?;
        for (txid, units) in receipts {
            crate::billing::record_direct_receipt(pool, id, &txid, units).await?;
        }
    }
    sqlx::query(
        "UPDATE crosschain_fees SET state='direct' WHERE invoice_id=? AND state NOT IN ('collected','provider_handled')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub fn start_worker(pool: SqlitePool, gateway: web::Data<Gateway>, key: String) {
    if !gateway.enabled {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_alert = 0;
        loop {
            interval.tick().await;
            if let Err(e) = crate::referrals::reconcile(&pool).await {
                tracing::error!(error=%e,"Referral reconciliation failed");
            }
            let now = Utc::now().timestamp();
            if now / 900 != last_alert {
                last_alert = now / 900;
                if let Ok(issues) = operations::issues(&pool).await {
                    if !issues.is_empty() { tracing::warn!(count=issues.len(), "Cross-chain payments require review; see admin payment operations"); }
                }
            }
            // Recover completed legacy payments even when the buyer closed the
            // page before the last provider poll. Unfunded quotes are not counted.
            let legacy = sqlx::query_scalar::<_, String>("SELECT a.invoice_id FROM crosschain_attempts a JOIN invoices i ON i.id=a.invoice_id
                WHERE i.status IN ('pending','underpaid','expired','detected','confirmed') AND a.payload IS NOT NULL AND a.last_checked<?
                AND NOT EXISTS(SELECT 1 FROM crosschain_fees f WHERE f.invoice_id=i.id)
                AND NOT EXISTS(SELECT 1 FROM crosschain_settlements s WHERE s.invoice_id=i.id)
                ORDER BY a.last_checked LIMIT 4").bind(now-600).fetch_all(&pool).await.unwrap_or_default();
            for id in legacy {
                let lease = sqlx::query("UPDATE crosschain_attempts SET last_checked=? WHERE invoice_id=? AND last_checked<?")
                    .bind(now).bind(&id).bind(now-600).execute(&pool).await;
                if matches!(lease, Ok(r) if r.rows_affected()==1) {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        reconcile(&pool, &gateway, &id, &key),
                    )
                    .await;
                }
            }
            let ids = sqlx::query_scalar::<_,String>("SELECT invoice_id FROM crosschain_fees WHERE state NOT IN ('collected','direct','provider_handled') AND next_check<=? ORDER BY next_check LIMIT 8")
                .bind(now).fetch_all(&pool).await;
            let Ok(ids) = ids else {
                tracing::error!("Could not load pending cross-chain fees");
                continue;
            };
            for id in ids {
                let lease = sqlx::query("UPDATE crosschain_fees SET next_check=? WHERE invoice_id=? AND state NOT IN ('collected','direct','provider_handled') AND next_check<=?")
                    .bind(now+600).bind(&id).bind(now).execute(&pool).await;
                if !matches!(lease,Ok(r) if r.rows_affected()==1) {
                    continue;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_secs(90),
                    reconcile(&pool, &gateway, &id, &key),
                )
                .await
                {
                    Ok(Ok(())) => {
                        tracing::info!(invoice_id=%id,"Cross-chain fee reconciliation completed")
                    }
                    _ => {
                        // Unverified/ambiguous attempts stay explicitly pending;
                        // never turn a provider outage into a merchant bill.
                        let _ = sqlx::query("UPDATE crosschain_fees SET failures=failures+1,next_check=?+MIN(3600,60*(1+failures)) WHERE invoice_id=? AND state NOT IN ('collected','direct','provider_handled')")
                            .bind(Utc::now().timestamp()).bind(&id).execute(&pool).await;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn confirmation_fixture(confirmations: i64) -> (String, Arc<std::sync::atomic::AtomicI64>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use std::sync::atomic::{AtomicI64, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicI64::new(confirmations));
        let state = count.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 { break; }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") { break; }
                }
                assert!(String::from_utf8_lossy(&request).starts_with("GET /v1/transactions/"));
                // A positive block height alongside zero confirmations models
                // stale explorer metadata after a transaction is reorged out.
                let body = json!({"data":{"block_height":100,"confirmations":state.load(Ordering::SeqCst)},"meta":{}}).to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (base, count)
    }

    async fn confidential_fee_fixture() -> (SqlitePool, String, Value, Value, Gateway) {
        use crate::repair_tests::{invoice, merchant, pool};
        let p = pool().await;
        merchant(&p, "m").await;
        merchant(&p, "referrer").await;
        sqlx::query("INSERT INTO merchant_referrals(merchant_id,referrer_id) VALUES ('m','referrer')")
            .execute(&p).await.unwrap();
        invoice(&p, "i", "m").await;
        invoice(&p, "other", "m").await;
        let key = "11".repeat(32);
        let q = json!({"confidentiality":"basic", "asset":{"assetId":"usdc", "symbol":"USDC", "blockchain":"base", "decimals":6},
            "amountOut":"1000", "depositAddress":"our-deposit", "refundAddress":"refund", "providerDeadline":"2099-01-01T00:00:00Z"});
        let response = signatures::signed(json!({"correlationId":"our-quote", "quoteRequest":{
            "dry":false, "confidentiality":"basic", "originAsset":"usdc",
            "destinationAsset":ZEC, "recipient":"test-address", "refundTo":"refund",
            "amount":"1000", "swapType":"EXACT_OUTPUT", "appFees":[
                {"recipient":"cipherscan.near","fee":125}, {"recipient":"provider.near","fee":20}]},
            "quote":{"depositAddress":"our-deposit", "amountOut":"1000"}}));
        let data = json!({"status":"SUCCESS", "quoteResponse":response,
            "swapDetails":{"destinationChainTxHashes":[{"hash":"ab".repeat(32)}], "intentHashes":[], "nearTxHashes":[]}});
        sqlx::query("INSERT INTO crosschain_attempts(invoice_id,state,payload) VALUES ('i','PROCESSING',?)")
            .bind(crate::crypto::encrypt(&q.to_string(), &key).unwrap()).execute(&p).await.unwrap();
        sqlx::query("INSERT INTO crosschain_fees(invoice_id,asset_id,recipient,total_bps,cipherpay_bps,state,correlation_id,quoted_input,failures) VALUES ('i','usdc','cipherscan.near',145,125,'pending','our-quote','5502400',5)")
            .execute(&p).await.unwrap();
        let (cipherscan_api_url, _) = confirmation_fixture(2).await;
        let g = Gateway { enabled:true, fee_recipient:Some("cipherscan.near".into()), key:String::new(), base:"http://127.0.0.1:1".into(),
            client:reqwest::Client::new(), confirmation_client:reqwest::Client::new(), cipherscan_api_url, tokens:Mutex::new(None),
            eligibility:Mutex::new(HashMap::new()), eligibility_slots:Semaphore::new(4) };
        (p, key, json!({"quote":q}), data, g)
    }

    #[tokio::test]
    async fn confidential_fee_is_handled_only_for_our_confirmed_full_delivery() {
        let (p, key, attempt, data, g) = confidential_fee_fixture().await;
        assert!(reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap_err().to_string().contains("Full ZEC delivery"));
        // A separate swap or direct ZEC transfer can settle the invoice, but its
        // txid is not our quote's delivery and cannot mark our NEAR fee handled.
        sqlx::query("INSERT INTO invoice_payments(invoice_id,txid,zatoshis) VALUES ('i',?,1000),('other',?,1000)")
            .bind("cd".repeat(32)).bind("ab".repeat(32)).execute(&p).await.unwrap();
        sqlx::query("UPDATE invoices SET status='confirmed',received_zatoshis=1000 WHERE id='i'")
            .execute(&p).await.unwrap();
        sqlx::query("UPDATE crosschain_attempts SET state='SUCCESS' WHERE invoice_id='i'").execute(&p).await.unwrap();
        assert!(reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap_err().to_string().contains("Full ZEC delivery"));
        assert_eq!(read_attempt(&p, "i", &key).await.unwrap().unwrap()["settlementVerified"], false);
        sqlx::query("INSERT INTO invoice_payments(invoice_id,txid,zatoshis) VALUES ('i',?,600)")
            .bind("ab".repeat(32)).execute(&p).await.unwrap();
        assert!(reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap_err().to_string().contains("Full ZEC delivery"));
        sqlx::query("UPDATE invoice_payments SET zatoshis=1000 WHERE invoice_id='i' AND txid=?")
            .bind("ab".repeat(32)).execute(&p).await.unwrap();
        sqlx::query("UPDATE invoices SET status='detected' WHERE id='i'").execute(&p).await.unwrap();
        assert!(reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap_err().to_string().contains("not confirmed"));
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "pending");
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_settlements").fetch_one(&p).await.unwrap(), 0);
        crate::billing::create_fee_entry(&p, "i", "m", 0.001, 0.01).await.unwrap();
        sqlx::query("UPDATE invoices SET status='confirmed' WHERE id='i'").execute(&p).await.unwrap();
        reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap();
        assert_eq!(read_attempt(&p, "i", &key).await.unwrap().unwrap()["settlementVerified"], true);
        let stored: (String, Option<String>, Option<String>, i64, i64) = sqlx::query_as("SELECT state,received_units,evidence,failures,next_check FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap();
        assert_eq!(stored, ("provider_handled".into(), None, None, 0, 0));
        let confirmation: String = sqlx::query_scalar("SELECT confirmation FROM crosschain_provider_confirmations WHERE invoice_id='i'").fetch_one(&p).await.unwrap();
        assert!(!confirmation.contains("our-deposit"));
        let audit: Value = serde_json::from_str(&crate::crypto::decrypt(&confirmation, &key).unwrap()).unwrap();
        assert_eq!(audit["policy"], "provider_handled");
        assert_eq!(audit["status"], data);
        // Both repeated provider data and worker retries are idempotent; no
        // further network call or independent credit evidence is required.
        reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap();
        reconcile(&p, &g, "i", &key).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_provider_confirmations").fetch_one(&p).await.unwrap(), 1);
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT confirmation FROM crosschain_provider_confirmations WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), confirmation);
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM deferred_payment_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "crosschain");
        crate::billing::create_fee_entry(&p, "i", "m", 0.001, 0.01).await.unwrap();
        crate::referrals::reconcile(&p).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM fee_ledger").fetch_one(&p).await.unwrap(), 0);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM referral_earnings").fetch_one(&p).await.unwrap(), 0);
        assert!(operations::issues(&p).await.unwrap().is_empty());
        let summary = merchant_summary(&p, "m", &key, true).await.unwrap();
        assert_eq!(summary["provider_handled_count"], 1);
        assert_eq!(summary["pending_count"], 0);
        assert_eq!(summary["collected_count"], 0);
        assert_eq!(summary["assets"], json!([]));
        assert_eq!(summary["recent"][0]["state"], "provider_handled");
        assert!(summary["recent"][0]["received_units"].is_null());
        // The accepted terminal policy also releases account-deletion holds;
        // existing invoice-owned encrypted metadata follows invoice deletion.
        crate::merchants::delete_merchant(&p, "m").await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_provider_confirmations").fetch_one(&p).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn confidential_success_rejects_altered_route_fee_or_quote() {
        let (p, key, attempt, data, g) = confidential_fee_fixture().await;
        sqlx::query("INSERT INTO invoice_payments(invoice_id,txid,zatoshis) VALUES ('i',?,1000)")
            .bind("ab".repeat(32)).execute(&p).await.unwrap();
        sqlx::query("UPDATE invoices SET status='confirmed',received_zatoshis=1000 WHERE id='i'").execute(&p).await.unwrap();
        for changes in [
            json!({"correlationId":"another-quote"}),
            json!({"quote":{"depositAddress":"another-deposit","amountOut":"1000"}}),
            json!({"quoteRequest":{"confidentiality":"public"}}),
            json!({"quoteRequest":{"recipient":"someone-else"}}),
            json!({"quoteRequest":{"appFees":[{"recipient":"attacker.near","fee":125},{"recipient":"provider.near","fee":20}]}}),
            json!({"quoteRequest":{"appFees":[{"recipient":"cipherscan.near","fee":124},{"recipient":"provider.near","fee":21}]}}),
            json!({"quoteRequest":{"appFees":[{"recipient":"cipherscan.near","fee":125},{"recipient":"provider.near","fee":21}]}}),
        ] {
            let mut response = data["quoteResponse"].clone();
            for (field, value) in changes.as_object().unwrap() {
                if field == "quoteRequest" {
                    for (subfield, value) in value.as_object().unwrap() {
                        response[field][subfield] = value.clone();
                    }
                } else { response[field] = value.clone(); }
            }
            let mut changed = data.clone();
            changed["quoteResponse"] = signatures::signed(response);
            assert!(reconcile_status(&p, &g, "i", &key, &attempt, &changed).await.is_err(), "{changes}");
        }
        let mut unsigned = data.clone();
        unsigned["quoteResponse"]["quote"]["amountOut"] = json!("1001");
        assert!(reconcile_status(&p, &g, "i", &key, &attempt, &unsigned).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "pending");
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_provider_confirmations").fetch_one(&p).await.unwrap(), 0);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_settlements").fetch_one(&p).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn independent_confirmation_never_confirms_an_unconfirmed_provider_delivery() {
        use std::sync::atomic::Ordering;
        let (p, key, attempt, data, mut g) = confidential_fee_fixture().await;
        let (base, confirmations) = confirmation_fixture(0).await;
        g.cipherscan_api_url = base;
        // Both outputs were observed, but only the independent direct payment
        // has confirmed. Overall invoice status cannot confirm our swap txid.
        sqlx::query("INSERT INTO invoice_payments(invoice_id,txid,zatoshis) VALUES ('i',?,1000),('i',?,1000)")
            .bind("cd".repeat(32)).bind("ab".repeat(32)).execute(&p).await.unwrap();
        sqlx::query("UPDATE invoices SET status='confirmed',received_zatoshis=1000 WHERE id='i'")
            .execute(&p).await.unwrap();
        for count in [0, -1] {
            confirmations.store(count, Ordering::SeqCst);
            assert!(reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap_err().to_string().contains("Full ZEC delivery"));
            assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "pending");
            assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_settlements").fetch_one(&p).await.unwrap(), 0);
            assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_provider_confirmations").fetch_one(&p).await.unwrap(), 0);
            assert_eq!(read_attempt(&p, "i", &key).await.unwrap().unwrap()["settlementVerified"], false);
        }
        confirmations.store(2, Ordering::SeqCst);
        reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "provider_handled");
        assert_eq!(read_attempt(&p, "i", &key).await.unwrap().unwrap()["settlementVerified"], true);
    }

    #[tokio::test]
    async fn direct_payment_to_invoice_never_marks_our_unfunded_swap_fee_handled() {
        let (p, key, mut attempt, mut data, g) = confidential_fee_fixture().await;
        sqlx::query("INSERT INTO invoice_payments(invoice_id,txid,zatoshis) VALUES ('i',?,1000)")
            .bind("cd".repeat(32)).execute(&p).await.unwrap();
        sqlx::query("UPDATE invoices SET status='confirmed',received_zatoshis=1000 WHERE id='i'").execute(&p).await.unwrap();
        crate::billing::create_fee_entry(&p, "i", "m", 0.001, 0.01).await.unwrap();
        data["status"] = json!("PENDING_DEPOSIT");
        assert!(reconcile_status(&p, &g, "i", &key, &attempt, &data).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "pending");
        // Only when the original deposit remains unfunded beyond its deadline
        // do we resolve the ordinary direct-ZEC obligation, without NEAR credit.
        attempt["quote"]["providerDeadline"] = json!("2020-01-01T00:00:00Z");
        reconcile_status(&p, &g, "i", &key, &attempt, &data).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "direct");
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM fee_ledger WHERE invoice_id='i' AND auto_collected=0").fetch_one(&p).await.unwrap(), 1);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_provider_confirmations").fetch_one(&p).await.unwrap(), 0);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_settlements").fetch_one(&p).await.unwrap(), 0);
        assert!(sqlx::query_scalar::<_,Option<String>>("SELECT received_units FROM crosschain_fees WHERE invoice_id='i'").fetch_one(&p).await.unwrap().is_none());
        assert_eq!(payment_methods(&p, "m").await.unwrap(), json!({"direct_zec":1,"crosschain":0,"verifying":0}));
        assert_eq!(merchant_summary(&p, "m", &key, true).await.unwrap()["provider_handled_count"], 0);
    }

    #[test]
    fn mixed_delivery_hashes_select_unique_zcash_candidates() {
        let zec = "ab".repeat(32);
        let evm = format!("0x{}", "cd".repeat(32));
        let details = json!({"destinationChainTxHashes":[
            {"hash":evm}, {"hash":zec.to_uppercase()}, {"hash":zec}
        ]});
        assert_eq!(zcash_deliveries(&details).unwrap(), vec![zec.clone()]);
        for entries in [json!([]), json!([{"hash":evm}]), json!([{"hash":zec},{"hash":null}]),
            json!([{"hash":""}]), json!([{"hash":"g".repeat(64)}]),
            json!([{"hash":"a".repeat(129)}]), json!(vec![json!({"hash":zec});17])] {
            assert!(zcash_deliveries(&json!({"destinationChainTxHashes":entries})).is_err());
        }
        assert!(zcash_deliveries(&json!({})).is_err());
    }

    #[actix_web::test]
    async fn mixed_refund_reconciles_only_after_this_invoices_full_zcash_receipt() {
        use actix_web::{App, HttpServer};
        use crate::repair_tests::{invoice, merchant, pool};
        let p = pool().await;
        merchant(&p, "m").await;
        invoice(&p, "i", "m").await;
        invoice(&p, "other", "m").await;
        let key = "11".repeat(32);
        let zec = "ab".repeat(32);
        let evm = format!("0x{}", "cd".repeat(32));
        let q = json!({"confidentiality":"basic", "asset":{"assetId":"usdc"},
            "amountOut":"1000", "depositAddress":"deposit", "refundAddress":"refund"});
        let signed = signatures::signed(json!({"quoteRequest":{
            "dry":false, "confidentiality":"basic", "originAsset":"usdc",
            "destinationAsset":ZEC, "recipient":"test-address", "refundTo":"refund",
            "amount":"1000", "swapType":"EXACT_OUTPUT"},
            "quote":{"depositAddress":"deposit", "amountOut":"1000"}}));
        let data = web::Data::new(json!({"status":"SUCCESS", "quoteResponse":signed,
            "swapDetails":{"destinationChainTxHashes":[
                {"hash":zec.to_uppercase()}, {"hash":evm}, {"hash":zec}]}}));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = HttpServer::new(move || App::new().app_data(data.clone())
            .route("/status", web::get().to(|data:web::Data<Value>| async move {
                HttpResponse::Ok().json(data.get_ref())
            }))
            .route("/v1/transactions/{txid}", web::get().to(|| async {
                HttpResponse::Ok().json(json!({"data":{"confirmations":2},"meta":{}}))
            }))).listen(listener).unwrap().run();
        let handle = server.handle();
        actix_web::rt::spawn(server);
        let g = Gateway { enabled:true, fee_recipient:None, key:String::new(), cipherscan_api_url:base.clone(), base,
            client:reqwest::Client::new(), confirmation_client:reqwest::Client::new(), tokens:Mutex::new(None),
            eligibility:Mutex::new(HashMap::new()), eligibility_slots:Semaphore::new(4) };
        sqlx::query("INSERT INTO crosschain_attempts(invoice_id,state,payload) VALUES ('i','PROCESSING',?)")
            .bind(crate::crypto::encrypt(&q.to_string(), &key).unwrap()).execute(&p).await.unwrap();
        // Provider SUCCESS and unrelated scanner receipts cannot settle this route.
        let error = reconcile(&p, &g, "i", &key).await.unwrap_err();
        assert!(error.to_string().contains("Full ZEC delivery"), "{error:#}");
        sqlx::query("INSERT INTO invoice_payments(invoice_id,txid,zatoshis) VALUES ('other',?,1000),('i',?,1000),('i',?,600)")
            .bind(&zec).bind("ef".repeat(32)).bind(&zec).execute(&p).await.unwrap();
        assert!(reconcile(&p, &g, "i", &key).await.unwrap_err().to_string().contains("Full ZEC delivery"));
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_settlements WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), 0);
        // Duplicate hashes never turn 600 zatoshis into a full 1,000-zatoshi receipt.
        sqlx::query("UPDATE invoice_payments SET zatoshis=1000 WHERE invoice_id='i' AND txid=?")
            .bind(&zec).execute(&p).await.unwrap();
        reconcile(&p, &g, "i", &key).await.unwrap();
        reconcile(&p, &g, "i", &key).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM crosschain_settlements WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), 1);
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_attempts WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), "SUCCESS");
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT txid FROM crosschain_deliveries WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), zec);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM fee_ledger WHERE invoice_id='i'").fetch_one(&p).await.unwrap(), 0);
        handle.stop(true).await;
    }

    #[test]
    fn confidential_fees_preserve_our_share_and_reject_changed_costs() {
        for ours in [125_u64, 75] {
            let mut body = json!({"confidentiality":"basic"});
            apply(&mut body, Some("cipherscan.near"));
            assert_eq!(body["appFees"][0]["fee"], 125);
            body["appFees"][0]["fee"] = json!(ours);
            assert_eq!(terms(&body).unwrap(), (ours + 20, ours));
            let mut response = json!({"quoteRequest":{"appFees":[
                {"recipient":"cipherscan.near","fee":ours},{"recipient":"provider.near","fee":20}
            ]}});
            validate_quote(&response, &body).unwrap();
            response["quoteRequest"]["appFees"][1]["fee"] = json!(21);
            assert!(validate_quote(&response, &body).is_err());
            response["quoteRequest"]["appFees"][1]["fee"] = json!(20);
            response["quoteRequest"]["appFees"][0]["fee"] = json!(ours * 2);
            assert!(validate_quote(&response, &body).is_err());
            response["quoteRequest"]["appFees"][0]["fee"] = json!(ours - 1);
            response["quoteRequest"]["appFees"][1]["fee"] = json!(21);
            assert!(validate_quote(&response, &body).is_err());
        }
    }

    #[tokio::test]
    async fn reservations_store_confidential_and_legacy_fee_terms() {
        use crate::repair_tests::{pool, merchant, invoice};
        let p = pool().await;
        merchant(&p, "m").await;
        for (id, mode, requested, expected) in [
            ("new", Some("basic"), 125, (145_i64, 125_i64)),
            ("discounted", Some("basic"), 75, (95, 75)),
            ("legacy", None, 250, (250, 125)),
        ] {
            invoice(&p, id, "m").await;
            let mut body = json!({"originAsset":"usdc","appFees":[{"recipient":"cipherscan.near","fee":requested}]});
            if let Some(mode) = mode { body["confidentiality"] = json!(mode); }
            let mut tx = p.begin().await.unwrap();
            reserve(&mut tx, id, &body).await.unwrap();
            tx.commit().await.unwrap();
            let stored: (i64, i64) = sqlx::query_as("SELECT total_bps,cipherpay_bps FROM crosschain_fees WHERE invoice_id=?")
                .bind(id).fetch_one(&p).await.unwrap();
            assert_eq!(stored, expected);
        }
    }

    #[test]
    fn resumed_confidential_routes_reject_downgrades_and_keep_legacy_payments() {
        let mut q = json!({"asset":{"assetId":"usdc"},"depositAddress":"deposit","amountOut":"42","refundAddress":"refund"});
        let mut data = json!({"status":"SUCCESS","quoteResponse":{"quote":{"depositAddress":"deposit","amountOut":"42"},
            "quoteRequest":{"originAsset":"usdc","destinationAsset":ZEC,"recipient":"merchant","refundTo":"refund","amount":"42","swapType":"EXACT_OUTPUT"}}});
        // Legacy attempts predate the setting and can still complete.
        validate_route(&data, &q, "merchant", None).unwrap();
        q["confidentiality"] = json!("basic");
        assert!(validate_route(&data, &q, "merchant", None).is_err());
        data["quoteResponse"]["quoteRequest"]["confidentiality"] = json!("basic");
        validate_route(&data, &q, "merchant", None).unwrap();
        data["quoteResponse"]["quoteRequest"]["confidentiality"] = json!("public");
        assert!(validate_route(&data, &q, "merchant", None).is_err());
    }

    #[test]
    fn status_without_correlation_remains_bound_to_the_original_route() {
        let q = json!({"asset":{"assetId":"usdc"},"depositAddress":"deposit","amountOut":"42","refundAddress":"refund"});
        let data = json!({"status":"SUCCESS","quoteResponse":{"quote":{"depositAddress":"deposit","amountOut":"42"},
            "quoteRequest":{"originAsset":"usdc","destinationAsset":ZEC,"recipient":"merchant","refundTo":"refund","amount":"42","swapType":"EXACT_OUTPUT"}}});
        validate_settlement(&data, &q, "merchant", Some("creation-id")).unwrap();
        for field in [
            "originAsset",
            "destinationAsset",
            "recipient",
            "refundTo",
            "amount",
            "swapType",
        ] {
            let mut bad = data.clone();
            bad["quoteResponse"]["quoteRequest"][field] = json!("replaced");
            assert!(
                validate_settlement(&bad, &q, "merchant", Some("creation-id")).is_err(),
                "{field}"
            );
        }
        for field in ["depositAddress", "depositMemo", "amountOut"] {
            let mut bad = data.clone();
            bad["quoteResponse"]["quote"][field] = json!("replaced");
            assert!(
                validate_settlement(&bad, &q, "merchant", Some("creation-id")).is_err(),
                "{field}"
            );
        }
        let mut correlated = data.clone();
        correlated["quoteResponse"]["correlationId"] = json!("creation-id");
        validate_settlement(&correlated, &q, "merchant", Some("creation-id")).unwrap();
        correlated["quoteResponse"]["correlationId"] = json!("other");
        assert!(validate_settlement(&correlated, &q, "merchant", Some("creation-id")).is_err());
    }

    #[tokio::test]
    async fn merchant_reporting_excludes_other_merchants_and_unfunded_quotes() {
        use crate::repair_tests::{invoice, merchant, pool};
        let p = pool().await;
        merchant(&p, "m").await;
        merchant(&p, "other").await;
        let key = "11".repeat(32);
        let payload = crate::crypto::encrypt(
            &json!({"asset":{"assetId":"usdc","symbol":"USDC","blockchain":"base","decimals":6}})
                .to_string(),
            &key,
        )
        .unwrap();
        for (id, owner, paid, attempt, settled, units) in [
            ("direct", "m", true, false, false, None),
            ("cross", "m", true, true, true, Some("69581")),
            ("unpaid", "m", false, true, false, None),
            ("unverified", "m", true, true, false, None),
            ("hidden", "other", true, true, true, Some("999999999")),
        ] {
            invoice(&p, id, owner).await;
            if paid {
                sqlx::query("UPDATE invoices SET status='confirmed' WHERE id=?")
                    .bind(id)
                    .execute(&p)
                    .await
                    .unwrap();
            }
            if attempt {
                sqlx::query("INSERT INTO crosschain_attempts(invoice_id,state,payload) VALUES (?,'PENDING_DEPOSIT',?)").bind(id).bind(&payload).execute(&p).await.unwrap();
                sqlx::query("INSERT INTO crosschain_fees(invoice_id,asset_id,recipient,total_bps,cipherpay_bps,state,received_units) VALUES (?,'usdc','cipherscan.near',250,125,?,?)")
                    .bind(id).bind(if units.is_some() {"collected"} else {"pending"}).bind(units).execute(&p).await.unwrap();
            }
            if settled {
                sqlx::query("INSERT INTO crosschain_settlements(invoice_id) VALUES (?)")
                    .bind(id)
                    .execute(&p)
                    .await
                    .unwrap();
            }
        }
        let methods = payment_methods(&p, "m").await.unwrap();
        assert_eq!(
            methods,
            json!({"direct_zec":1,"crosschain":1,"verifying":1})
        );
        // Independent of direct ZEC fee enablement; actual received units only.
        let summary = merchant_summary(&p, "m", &key, true).await.unwrap();
        assert_eq!(summary["collected_count"], 1);
        assert_eq!(summary["pending_count"], 1);
        assert_eq!(summary["assets"][0]["received_units"], "69581");
        assert_eq!(summary["recent"].as_array().unwrap().len(), 2);
        assert!(!summary.to_string().contains("hidden"));
        let disabled = merchant_summary(&p, "m", &key, false).await.unwrap();
        assert_eq!(disabled["collected_count"], 1); // History survives a feature toggle.
    }

    fn request() -> Value {
        let mut body = json!({"originAsset":"nep141:usdc.near"});
        apply(&mut body, Some("cipherscan.near"));
        body
    }

    #[test]
    fn validates_provider_rewritten_split_and_rejects_silent_price_changes() {
        let body = request();
        assert_eq!(body["appFees"][0]["fee"], 250);
        let mut response = json!({"quoteRequest":{"appFees":[
            {"recipient":"cipherscan.near","fee":125},{"recipient":"provider.near","fee":125}
        ]}});
        validate_quote(&response, &body).unwrap();
        response["quoteRequest"]["appFees"][1]["fee"] = json!(145);
        assert!(validate_quote(&response, &body).is_err());
        response["quoteRequest"]["appFees"][1]["fee"] = json!(125);
        response["quoteRequest"]["appFees"][0]["recipient"] = json!("other.near");
        assert!(validate_quote(&response, &body).is_err());
        assert!(validate_quote(&json!({}), &body).is_err());
        validate_quote(&json!({}), &json!({})).unwrap(); // old fee-free quotes
    }

    fn receipt() -> Value {
        // DIP4 transfer shape verified against a finalized 1Click settlement.
        let event = json!({"standard":"dip4","version":"0.4.3","event":"transfer","data":[
            {"intent_hash":"this-intent","account_id":"source.near","receiver_id":"cipherscan.near","tokens":{"nep141:usdc.near":"71234"}},
            {"intent_hash":"another-intent","account_id":"source.near","receiver_id":"cipherscan.near","tokens":{"nep141:usdc.near":"999999"}}
        ]});
        json!({"final_execution_status":"FINAL","transaction":{"hash":"tx"},"status":{"SuccessValue":""},
            "receipts_outcome":[{"id":"receipt","outcome":{"executor_id":"intents.near","status":{"SuccessValue":""},"logs":[format!("EVENT_JSON:{event}")]}}]})
    }

    #[test]
    fn collection_requires_final_contract_receipt_bound_to_this_swap_and_asset() {
        let mut r = receipt();
        let intents = vec!["this-intent".into()];
        let c = credits(&r, "tx", &intents, "cipherscan.near", "nep141:usdc.near").unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].units, 71234);
        assert!(credits(
            &r,
            "other-tx",
            &intents,
            "cipherscan.near",
            "nep141:usdc.near"
        )
        .is_err());
        assert!(
            credits(&r, "tx", &intents, "cipherscan.near", "nep141:other.near")
                .unwrap()
                .is_empty()
        );
        r["receipts_outcome"][0]["outcome"]["executor_id"] = json!("fake.near");
        assert!(
            credits(&r, "tx", &intents, "cipherscan.near", "nep141:usdc.near")
                .unwrap()
                .is_empty()
        );
        r["receipts_outcome"][0]["outcome"]["executor_id"] = json!("intents.near");
        r["receipts_outcome"][0]["outcome"]["status"] = json!({"Failure":{}});
        assert!(
            credits(&r, "tx", &intents, "cipherscan.near", "nep141:usdc.near")
                .unwrap()
                .is_empty()
        );
        r["final_execution_status"] = json!("EXECUTED_OPTIMISTIC");
        assert!(credits(&r, "tx", &intents, "cipherscan.near", "nep141:usdc.near").is_err());
    }

    #[tokio::test]
    async fn pending_or_collected_swap_never_enters_merchant_bill_and_proof_cannot_be_reused() {
        let p = crate::repair_tests::pool().await;
        crate::repair_tests::merchant(&p, "m").await;
        for id in ["swap", "second", "direct"] {
            crate::repair_tests::invoice(&p, id, "m").await;
        }
        for id in ["swap", "second"] {
            let mut tx = p.begin().await.unwrap();
            reserve(&mut tx, id, &request()).await.unwrap();
            tx.commit().await.unwrap();
        }
        for _ in 0..2 {
            crate::billing::create_fee_entry(&p, "swap", "m", 0.001, 0.01)
                .await
                .unwrap();
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fee_ledger")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let c = credits(
            &receipt(),
            "tx",
            &["this-intent".into()],
            "cipherscan.near",
            "nep141:usdc.near",
        )
        .unwrap();
        record(&p, "swap", &c, "encrypted-test-evidence")
            .await
            .unwrap();
        record(&p, "swap", &c, "encrypted-test-evidence")
            .await
            .unwrap();
        assert!(record(&p, "second", &c, "encrypted-test-evidence")
            .await
            .is_err());
        let actual: Option<String> = sqlx::query_scalar(
            "SELECT received_units FROM crosschain_fees WHERE invoice_id='swap'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(actual.as_deref(), Some("71234"));
        let pending: Option<String> = sqlx::query_scalar(
            "SELECT received_units FROM crosschain_fees WHERE invoice_id='second'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(pending, None);
        crate::billing::create_fee_entry(&p, "swap", "m", 0.001, 0.01)
            .await
            .unwrap();
        crate::billing::create_fee_entry(&p, "direct", "m", 0.001, 0.01)
            .await
            .unwrap();
        let billed: Vec<String> = sqlx::query_scalar("SELECT invoice_id FROM fee_ledger")
            .fetch_all(&p)
            .await
            .unwrap();
        assert_eq!(billed, vec!["direct"]);
    }
}
