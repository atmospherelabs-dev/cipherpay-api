//! Invoice-bound 1Click payments. Provider status is informational: only the
//! Zcash scanner may settle an invoice. Never retry an ambiguous quote creation.
use actix_web::{web, HttpResponse};
use anyhow::{bail, ensure, Context};
use chrono::{Duration, Utc};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::time::Instant;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Semaphore};

pub mod fees;
pub mod security;
pub mod operations;
mod signatures;

const ZEC: &str = "nep141:zec.omft.near";
const BASE: &str = "https://1click.chaindefuser.com/v0";
const CONFIDENTIALITY: &str = "basic";

#[derive(Debug, thiserror::Error)]
#[error("Payment is below the route's minimum output")]
struct BelowMinimumOutput(u128);

fn minimum_output_error(status: reqwest::StatusCode, body: &Value) -> Option<BelowMinimumOutput> {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return None;
    }
    let units = body
        .get("message")?
        .as_str()?
        .strip_prefix("Amount is too low for bridge, try at least ")?;
    if units.is_empty() || !units.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let minimum = units.parse::<u128>().ok()?;
    (minimum > 0).then_some(BelowMinimumOutput(minimum))
}

#[derive(Debug, thiserror::Error)]
#[error("Payment is below the provider's minimum swap amount")]
struct BelowMinimumSwapUsd(String);

fn minimum_swap_usd_error(
    status: reqwest::StatusCode,
    body: &Value,
) -> Option<BelowMinimumSwapUsd> {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return None;
    }
    let amount = body
        .get("message")?
        .as_str()?
        .strip_prefix("Temporary swap limits: minimum swap amount is $")?;
    let mut parts = amount.split('.');
    let whole = parts.next()?;
    if whole.is_empty() || whole.len() > 9 || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(fraction) = parts.next() {
        if fraction.is_empty()
            || fraction.len() > 2
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
    }
    if parts.next().is_some() || !amount.bytes().any(|b| matches!(b, b'1'..=b'9')) {
        return None;
    }
    Some(BelowMinimumSwapUsd(amount.into()))
}

pub struct Gateway {
    enabled: bool,
    fee_recipient: Option<String>,
    key: String,
    base: String,
    client: reqwest::Client,
    confirmation_client: reqwest::Client,
    cipherscan_api_url: String,
    tokens: Mutex<Option<(Instant, Vec<Asset>)>>,
    eligibility: Mutex<HashMap<String, Arc<Mutex<Option<(Instant, Value)>>>>>,
    eligibility_slots: Semaphore,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub asset_id: String,
    pub symbol: String,
    pub blockchain: String,
    pub decimals: u32,
    #[serde(default)]
    pub contract_address: Option<String>,
}

// Explicit token identities prevent lookalike symbols and wrapped ZEC from
// becoming checkout assets. The live registry remains the availability source.
fn curated(id: &str) -> bool {
    matches!(
        id,
        "nep141:base-0x833589fcd6edb6e08f4c7c32d4f71b54bda02913.omft.near"
            | "nep141:eth-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.omft.near"
            | "nep141:arb-0xaf88d065e77c8cc2239327c5edb3a432268e5831.omft.near"
            | "nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near"
            | "nep141:eth-0xdac17f958d2ee523a2206206994597c13d831ec7.omft.near"
            | "nep141:tron-d28a265909efecdcee7c5028585214ea0b96f015.omft.near"
            | "nep141:sol-c800a4bd850783ccb82c2b2c7e84175443606352.omft.near"
            | "nep141:eth.omft.near"
            | "nep141:base.omft.near"
            | "nep141:arb.omft.near"
            | "nep141:btc.omft.near"
            | "nep141:sol.omft.near"
            | "nep141:tron.omft.near"
    )
}

impl Gateway {
    pub fn collects_fees(&self) -> bool {
        self.enabled && self.fee_recipient.is_some()
    }

    pub fn from_env(config: &crate::config::Config) -> anyhow::Result<Self> {
        let key = std::env::var("NEAR_ONECLICK_API_KEY").unwrap_or_default();
        let enabled = std::env::var("NEAR_CROSSCHAIN_ENABLED").as_deref() == Ok("true");
        if enabled {
            ensure!(!config.is_testnet(), "1Click checkout is mainnet only");
            ensure!(!key.is_empty(), "NEAR_ONECLICK_API_KEY is required");
            // Refund addresses and deposit instructions are encrypted at rest.
            crate::crypto::encrypt("crosschain", &config.encryption_key)?;
            ensure!(
                !config.encryption_key.is_empty(),
                "Cross-chain checkout requires encryption"
            );
        }
        let fee_recipient = std::env::var("NEAR_APP_FEE_RECIPIENT")
            .ok()
            .filter(|s| !s.is_empty());
        if let Some(recipient) = &fee_recipient {
            ensure!(
                enabled && !key.is_empty(),
                "App fees require authenticated cross-chain checkout"
            );
            ensure!(
                recipient == "cipherscan.near",
                "Unapproved app fee recipient"
            );
        }
        // Internal explorer credentials must never accompany provider requests.
        let mut confirmation_headers = reqwest::header::HeaderMap::new();
        if let Some(key) = &config.cipherscan_service_key {
            let mut value = reqwest::header::HeaderValue::from_str(key)?;
            value.set_sensitive(true);
            confirmation_headers.insert("X-Service-Key", value);
        }
        Ok(Self {
            enabled,
            fee_recipient,
            key,
            base: BASE.into(),
            client: reqwest::Client::builder()
                .user_agent("CipherPay/1.0")
                .timeout(std::time::Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            confirmation_client: reqwest::Client::builder()
                .user_agent("CipherPay/1.0")
                .default_headers(confirmation_headers)
                .timeout(std::time::Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            cipherscan_api_url: config.cipherscan_api_url.clone(),
            tokens: Mutex::new(None),
            eligibility: Mutex::new(HashMap::new()),
            eligibility_slots: Semaphore::new(4),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.request(method, format!("{}{path}", self.base));
        if self.key.is_empty() {
            req
        } else {
            req.bearer_auth(&self.key)
        }
    }

    async fn assets(&self) -> anyhow::Result<Vec<Asset>> {
        let mut cache = self.tokens.lock().await;
        if let Some((at, tokens)) = cache.as_ref() {
            if at.elapsed().as_secs() < 600 {
                return Ok(tokens.clone());
            }
        }
        let all: Vec<Asset> = self
            .request(reqwest::Method::GET, "/tokens")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            all.iter()
                .any(|a| a.asset_id == ZEC && a.decimals == 8 && a.blockchain == "zec"),
            "ZEC unavailable"
        );
        let mut tokens: Vec<_> = all
            .into_iter()
            .filter(|a| curated(&a.asset_id) && a.decimals <= 18)
            .collect();
        tokens.sort_by_key(|a| {
            let rank = match a.symbol.as_str() {
                "USDC" => 0,
                "USDT" => 1,
                "ETH" => 2,
                "BTC" => 3,
                "SOL" => 4,
                _ => 5,
            };
            let chain = match a.blockchain.as_str() {
                "base" => 0,
                "sol" => 1,
                "tron" => 2,
                "eth" => 3,
                _ => 4,
            };
            (rank, chain)
        });
        *cache = Some((Instant::now(), tokens.clone()));
        Ok(tokens)
    }

    async fn quote(&self, body: &Value) -> anyhow::Result<Value> {
        let _permit = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.eligibility_slots.acquire(),
        )
        .await??;
        let res = self
            .request(reqwest::Method::POST, "/quote")
            .json(body)
            .send()
            .await?;
        // Extract only a known numeric minimum, never echo provider messages
        // that may contain destination/refund data. All requests are EXACT_OUTPUT.
        let status = res.status();
        if !status.is_success() {
            let body: Value = res.json().await.unwrap_or(Value::Null);
            if let Some(error) = minimum_output_error(status, &body) {
                return Err(error.into());
            }
            if let Some(error) = minimum_swap_usd_error(status, &body) {
                return Err(error.into());
            }
            bail!("No quote available for this route and amount");
        }
        let response: Value = res.json().await?;
        signatures::verify_fresh(&response)?;
        validate_confidentiality(&response, body)?;
        Ok(response)
    }
}

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("ALTER TABLE invoices ADD COLUMN exact_payment INTEGER NOT NULL DEFAULT 0")
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE TABLE crosschain_attempts (
        invoice_id TEXT PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
        state TEXT NOT NULL, payload TEXT, last_checked INTEGER NOT NULL DEFAULT 0, last_success INTEGER NOT NULL DEFAULT 0,
        send_token TEXT, sent_txid TEXT,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
    )").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteInput {
    asset_id: String,
    refund_address: String,
}

fn valid_refund(chain: &str, address: &str) -> bool {
    if address.len() > 120 || address.trim() != address {
        return false;
    }
    use sha2::Digest;
    let checked_base58 = |version: u8| {
        let Ok(bytes) = bs58::decode(address).into_vec() else {
            return false;
        };
        bytes.len() == 25
            && bytes[0] == version
            && sha2::Sha256::digest(sha2::Sha256::digest(&bytes[..21]))[..4] == bytes[21..]
    };
    match chain {
        "eth" | "base" | "arb" => {
            if address.len() != 42
                || !address.starts_with("0x")
                || !address[2..].bytes().all(|b| b.is_ascii_hexdigit())
                || !address[2..].bytes().any(|b| b != b'0')
            {
                return false;
            }
            let raw = &address[2..];
            if raw == raw.to_lowercase() || raw == raw.to_uppercase() {
                return true;
            }
            let hash = hex::encode(sha3::Keccak256::digest(raw.to_lowercase().as_bytes()));
            raw.bytes().zip(hash.bytes()).all(|(c, h)| {
                !c.is_ascii_alphabetic()
                    || c.is_ascii_uppercase() == ((h as char).to_digit(16).unwrap() >= 8)
            })
        }
        "sol" => bs58::decode(address)
            .into_vec()
            .is_ok_and(|bytes| bytes.len() == 32 && bytes.iter().any(|b| *b != 0)),
        "tron" => checked_base58(0x41),
        "btc" => {
            checked_base58(0)
                || checked_base58(5)
                || bech32::segwit::decode(address)
                    .is_ok_and(|(hrp, _, _)| hrp == bech32::Hrp::parse("bc").unwrap())
        }
        _ => false,
    }
}

fn validate_invoice(inv: &crate::invoices::Invoice) -> anyhow::Result<()> {
    ensure!(
        inv.status == "pending" && inv.received_zatoshis == 0 && inv.price_zatoshis > 0,
        "Invoice is not available for a new payment"
    );
    ensure!(
        chrono::DateTime::parse_from_rfc3339(&inv.expires_at)? > Utc::now(),
        "Invoice has expired"
    );
    let receiver = crate::invoices::matching::orchard_receiver(&inv.payment_address);
    ensure!(
        receiver.is_some() && receiver == inv.orchard_receiver_hex,
        "Invoice needs a unique shielded address"
    );
    ensure!(
        inv.payment_address.starts_with("u1"),
        "Mainnet address required"
    );
    Ok(())
}

fn quote_body(
    inv: &crate::invoices::Invoice,
    input: &QuoteInput,
    asset: &Asset,
    dry: bool,
) -> anyhow::Result<Value> {
    validate_invoice(inv)?;
    ensure!(
        valid_refund(&asset.blockchain, &input.refund_address),
        "Enter a valid refund address on the selected network"
    );
    let minutes = if asset.blockchain == "btc" { 120 } else { 30 };
    Ok(json!({
        "dry": dry, "swapType": "EXACT_OUTPUT", "slippageTolerance": 100,
        "originAsset": asset.asset_id, "destinationAsset": ZEC,
        "amount": inv.price_zatoshis.to_string(),
        "recipient": inv.payment_address, "recipientType": "DESTINATION_CHAIN",
        "refundTo": input.refund_address, "refundType": "ORIGIN_CHAIN",
        "depositType": "ORIGIN_CHAIN", "depositMode": "SIMPLE",
        "confidentiality": CONFIDENTIALITY,
        "deadline": std::cmp::min(Utc::now() + Duration::minutes(minutes), chrono::DateTime::parse_from_rfc3339(&inv.expires_at)?.with_timezone(&Utc)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "quoteWaitingTimeMs": 3000
    }))
}

fn positive_units(value: &Value) -> anyhow::Result<u128> {
    let raw = value.as_str().context("Missing amount")?;
    ensure!(
        !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()),
        "Invalid integer amount"
    );
    let n: u128 = raw.parse()?;
    ensure!(n > 0, "Invalid amount");
    Ok(n)
}

// 1Click's signed-field selection does not cover confidentiality. Check the
// authenticated HTTPS response separately; a signature alone cannot attest it.
// Saved legacy quotes omit the field and retain their original routing terms.
fn validate_confidentiality(response: &Value, expected: &Value) -> anyhow::Result<()> {
    if let Some(mode) = expected.get("confidentiality") {
        ensure!(mode == CONFIDENTIALITY, "Unsupported confidentiality mode");
        ensure!(
            response["quoteRequest"]["confidentiality"] == *mode,
            "Provider did not confirm confidential routing"
        );
    }
    Ok(())
}

fn checked_quote(
    response: &Value,
    body: &Value,
    asset: &Asset,
    dry: bool,
) -> anyhow::Result<Value> {
    validate_confidentiality(response, body)?;
    fees::validate_quote(response, body)?;
    let q = &response["quote"];
    let target = positive_units(&body["amount"])?;
    ensure!(
        positive_units(&q["amountOut"])? == target,
        "Quote does not cover the invoice"
    );
    if !q["minAmountOut"].is_null() {
        ensure!(
            positive_units(&q["minAmountOut"])? >= target,
            "Quote permits underpayment"
        );
    }
    positive_units(&q["amountIn"])?;
    // If present, reject an echoed request for another invoice/route.
    if let Some(echo) = response.get("quoteRequest") {
        for key in [
            "recipient",
            "amount",
            "originAsset",
            "destinationAsset",
            "swapType",
            "refundTo",
        ] {
            ensure!(echo[key] == body[key], "Quote request mismatch");
        }
    }
    let mut payment_deadline = q["deadline"].clone();
    if !dry {
        let deposit = q["depositAddress"]
            .as_str()
            .context("Missing deposit address")?;
        ensure!(
            valid_refund(&asset.blockchain, deposit),
            "Invalid deposit address"
        );
        let deadline = q["deadline"].as_str().context("Missing quote deadline")?;
        let until = chrono::DateTime::parse_from_rfc3339(deadline)?;
        let requested = chrono::DateTime::parse_from_rfc3339(
            body["deadline"]
                .as_str()
                .context("Missing requested deadline")?,
        )?;
        // 1Click may extend committed ZEC quote lifetimes (currently by three
        // days). Keep our short funding window separate from provider recovery.
        ensure!(
            until > Utc::now() && until <= requested + Duration::days(7),
            "Invalid quote deadline"
        );
        let mut funding_until = until.min(requested);
        if !q["timeWhenInactive"].is_null() {
            let inactive = chrono::DateTime::parse_from_rfc3339(
                q["timeWhenInactive"]
                    .as_str()
                    .context("Invalid inactive time")?,
            )?;
            funding_until = funding_until.min(inactive);
        }
        ensure!(funding_until > Utc::now(), "Quote is already inactive");
        payment_deadline =
            json!(funding_until.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    }
    Ok(json!({
        "asset": asset, "amountIn": q["amountIn"], "amountOut": q["amountOut"],
        "timeEstimate": q["timeEstimate"], "depositAddress": q["depositAddress"],
        "depositMemo": q["depositMemo"], "deadline": payment_deadline, "providerDeadline": q["deadline"],
        "refundAddress": body["refundTo"], "confidentiality": body["confidentiality"]
    }))
}

fn error(status: actix_web::http::StatusCode, message: &str) -> HttpResponse {
    HttpResponse::build(status)
        .insert_header(("Cache-Control", "no-store"))
        .json(json!({"error": message}))
}

pub async fn assets(g: web::Data<Gateway>) -> HttpResponse {
    if !g.enabled {
        return HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(json!({"enabled": false, "assets": []}));
    }
    match g.assets().await {
        Ok(assets) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(json!({"enabled": true, "assets": assets})),
        Err(_) => error(
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            "Crypto payment options are temporarily unavailable",
        ),
    }
}

// Provider-required refund placeholders for DRY availability checks only. These
// never reach the browser or any stored/fundable quote. Actual checkout still
// requires the buyer's address. Do not reuse this function for quote creation.
fn eligibility_body(inv: &crate::invoices::Invoice, asset: &Asset) -> anyhow::Result<Value> {
    let refund = match asset.blockchain.as_str() {
        "eth" | "base" | "arb" => "0x1111111111111111111111111111111111111111",
        "sol" => "5dTEX4m5e4B64xuDr83gaCSVa3W7GeqMmZPj3RoT5cQs",
        "btc" => "1BitcoinEaterAddressDontSendf59kuE",
        "tron" => "T9yD14Nj9j7xAB4dbGeiX9h8unkKHxuWwb",
        _ => bail!("Unsupported availability route"),
    };
    quote_body(
        inv,
        &QuoteInput {
            asset_id: asset.asset_id.clone(),
            refund_address: refund.into(),
        },
        asset,
        true,
    )
}

fn route_eligibility(result: anyhow::Result<Value>, body: &Value, asset: &Asset) -> Value {
    match result {
        Ok(q)
            if q["quote"]["depositAddress"].is_null()
                && checked_quote(&q, body, asset, true).is_ok() =>
        {
            json!({"assetId":asset.asset_id,"state":"AVAILABLE"})
        }
        Err(e) if e.downcast_ref::<BelowMinimumOutput>().is_some() => {
            let minimum = e.downcast_ref::<BelowMinimumOutput>().unwrap().0;
            // Only call it a minimum failure if that minimum actually exceeds
            // this invoice. Other provider/validation failures remain unknown.
            if body["amount"]
                .as_str()
                .and_then(|s| s.parse::<u128>().ok())
                .is_some_and(|amount| amount < minimum)
            {
                json!({"assetId":asset.asset_id,"state":"BELOW_MINIMUM","minimumOutputZatoshis":minimum.to_string()})
            } else {
                json!({"assetId":asset.asset_id,"state":"UNKNOWN"})
            }
        }
        Err(e) if e.downcast_ref::<BelowMinimumSwapUsd>().is_some() => {
            json!({"assetId":asset.asset_id,"state":"BELOW_MINIMUM","minimumSwapUsd":e.downcast_ref::<BelowMinimumSwapUsd>().unwrap().0})
        }
        _ => json!({"assetId":asset.asset_id,"state":"UNKNOWN"}),
    }
}

pub async fn eligibility(
    pool: web::Data<SqlitePool>,
    g: web::Data<Gateway>,
    path: web::Path<String>,
) -> HttpResponse {
    if !g.enabled {
        return error(
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            "Cross-chain payments are not enabled",
        );
    }
    let inv = match crate::invoices::get_invoice(&pool, &path).await {
        Ok(Some(inv)) => inv,
        Ok(None) => return error(actix_web::http::StatusCode::NOT_FOUND, "Invoice not found"),
        Err(_) => {
            return error(
                actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                "Could not check availability",
            )
        }
    };
    if !operations::enabled(&pool, &path).await.unwrap_or(false) {
        return HttpResponse::Ok().json(json!({"enabled":false,"state":"DISABLED","routes":[],"hasUnknown":false}));
    }
    if let Err(e) = validate_invoice(&inv) {
        return error(actix_web::http::StatusCode::BAD_REQUEST, &e.to_string());
    }
    // Preserve active attempts: a new availability result must never offer a
    // second route or turn an existing payment into a direct ZEC payment.
    match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM crosschain_attempts WHERE invoice_id=?)",
    )
    .bind(path.as_str())
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(true) => {
            return error(
                actix_web::http::StatusCode::CONFLICT,
                "Payment already started",
            )
        }
        Err(_) => {
            return error(
                actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                "Could not check payment state",
            )
        }
        _ => {}
    }
    let key = format!("{}:{}:{}", inv.id, inv.price_zatoshis, inv.payment_address);
    let slot = {
        let mut entries = g.eligibility.lock().await;
        if entries.len() >= 256 && !entries.contains_key(&key) {
            entries.retain(|_, entry| Arc::strong_count(entry) > 1);
            if entries.len() >= 256 {
                return error(
                    actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Availability is busy; retry shortly",
                );
            }
        }
        entries
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    // Single-flight per invoice and bounded provider concurrency across invoices.
    let mut cached =
        match tokio::time::timeout(std::time::Duration::from_secs(18), slot.lock()).await {
            Ok(lock) => lock,
            Err(_) => {
                return error(
                    actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Availability is busy; retry shortly",
                )
            }
        };
    if let Some((at, value)) = cached.as_ref() {
        let ttl = if value["hasUnknown"] == true { 5 } else { 30 };
        if at.elapsed().as_secs() < ttl {
            return HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(value);
        }
    }
    let assets = match tokio::time::timeout(std::time::Duration::from_secs(5), g.assets()).await {
        Ok(Ok(assets)) => assets,
        _ => {
            return error(
                actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                "Could not check availability",
            )
        }
    };
    let cipherpay_bps = match crate::referrals::service_bps(&pool, &inv.id).await {
        Ok(bps) => bps + 25,
        Err(_) => {
            return error(
                actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                "Fee policy unavailable",
            )
        }
    };
    let mut routes: Vec<Value> = assets
        .iter()
        .map(|a| json!({"assetId":a.asset_id,"state":"UNKNOWN"}))
        .collect();
    let mut checks = stream::iter(assets.iter().enumerate().map(|(i, asset)| {
        let g = &g;
        let inv = &inv;
        async move {
            let result = async {
                let mut body = eligibility_body(inv, asset)?;
                fees::apply(&mut body, g.fee_recipient.as_deref());
                if g.fee_recipient.is_some() {
                    body["appFees"][0]["fee"] = json!(cipherpay_bps);
                }
                Ok::<_, anyhow::Error>(route_eligibility(g.quote(&body).await, &body, asset))
            }
            .await;
            (
                i,
                result.unwrap_or_else(|_| json!({"assetId":asset.asset_id,"state":"UNKNOWN"})),
            )
        }
    }))
    .buffer_unordered(4);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(16);
    while let Ok(Some((i, route))) = tokio::time::timeout_at(deadline, checks.next()).await {
        routes[i] = route;
    }
    let has_unknown = routes.iter().any(|r| r["state"] == "UNKNOWN") || routes.is_empty();
    let state = if routes.iter().any(|r| r["state"] == "AVAILABLE") {
        "AVAILABLE"
    } else if !has_unknown {
        "UNAVAILABLE"
    } else {
        "UNKNOWN"
    };
    let value = json!({"state":state,"routes":routes,"hasUnknown":has_unknown,"checkedAt":Utc::now().to_rfc3339(),"validForSeconds":if has_unknown {5} else {30}});
    *cached = Some((Instant::now(), value.clone()));
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(value)
}

async fn prepare(
    pool: &SqlitePool,
    g: &Gateway,
    id: &str,
    input: &QuoteInput,
    dry: bool,
) -> anyhow::Result<(Value, Asset)> {
    ensure!(g.enabled, "Cross-chain payments are not enabled");
    let inv = crate::invoices::get_invoice(pool, id)
        .await?
        .context("Invoice not found")?;
    ensure!(operations::enabled(pool, id).await?, "The merchant accepts ZEC only");
    let asset = g
        .assets()
        .await?
        .into_iter()
        .find(|a| a.asset_id == input.asset_id)
        .context("Unsupported asset")?;
    let mut body = quote_body(&inv, input, &asset, dry)?;
    fees::apply(&mut body, g.fee_recipient.as_deref());
    if g.fee_recipient.is_some() {
        body["appFees"][0]["fee"] =
            json!(crate::referrals::service_bps(pool, id).await? + 25);
    }
    Ok((body, asset))
}

pub async fn preview(
    pool: web::Data<SqlitePool>,
    g: web::Data<Gateway>,
    path: web::Path<String>,
    input: web::Json<QuoteInput>,
) -> HttpResponse {
    let (body, asset) = match prepare(&pool, &g, &path, &input, true).await {
        Ok(v) => v,
        Err(e) => return error(actix_web::http::StatusCode::BAD_REQUEST, &e.to_string()),
    };
    match g
        .quote(&body)
        .await
        .and_then(|q| checked_quote(&q, &body, &asset, true))
    {
        Ok(q) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(q),
        Err(e) if e.downcast_ref::<BelowMinimumOutput>().is_some() => {
            let minimum = e.downcast_ref::<BelowMinimumOutput>().unwrap().0;
            HttpResponse::BadRequest()
                .insert_header(("Cache-Control", "no-store"))
                .json(json!({"error": "This invoice is below the cross-chain route minimum. Pay with ZEC or ask the merchant for a larger invoice.", "code": "BELOW_MINIMUM_OUTPUT", "minimumOutputZatoshis": minimum.to_string()}))
        }
        Err(_) => error(
            actix_web::http::StatusCode::BAD_GATEWAY,
            "No exact-output quote available. Try another network or amount.",
        ),
    }
}

async fn reserve(
    pool: &SqlitePool,
    id: &str,
    deadline: &str,
    fee_body: Option<&Value>,
) -> anyhow::Result<bool> {
    // One durable attempt per invoice, including after a timeout/crash. A second
    // browser can never create competing deposit addresses. No provider call is
    // made while a SQLite transaction is held.
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "INSERT OR IGNORE INTO crosschain_attempts (invoice_id,state,price_expires_at)
        SELECT id,'CREATING',expires_at FROM invoices WHERE id=? AND status='pending'
        AND received_zatoshis=0 AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')
        AND EXISTS(SELECT 1 FROM merchants m WHERE m.id=invoices.merchant_id AND m.crosschain_enabled=1)",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        return Ok(false);
    }
    chrono::DateTime::parse_from_rfc3339(deadline)?;
    sqlx::query("UPDATE invoices SET exact_payment=1 WHERE id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if let Some(body) = fee_body {
        fees::reserve(&mut tx, id, body).await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn create(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    g: web::Data<Gateway>,
    config: web::Data<crate::config::Config>,
    path: web::Path<String>,
    input: web::Json<QuoteInput>,
) -> HttpResponse {
    let id = path.into_inner();
    if !security::authorized(&req, &pool, &id).await {
        return error(
            actix_web::http::StatusCode::FORBIDDEN,
            "Open the full payment link provided by the merchant to start or resume this payment.",
        );
    }
    // Return the original attempt on duplicate requests, even if invoice is paid.
    match read_attempt(&pool, &id, &config.encryption_key).await {
        Ok(Some(mut a)) => {
            a["canManage"] = json!(true);
            if !a["quote"].is_null()
                && (a["quote"]["asset"]["assetId"] != input.asset_id
                    || a["quote"]["refundAddress"] != input.refund_address)
            {
                return error(actix_web::http::StatusCode::CONFLICT, "An existing payment uses different token or refund details. Resume it from the original checkout.");
            }
            return HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(a);
        }
        Err(_) => {
            return error(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore payment",
            )
        }
        _ => {}
    }
    let (body, asset) = match prepare(&pool, &g, &id, &input, false).await {
        Ok(v) => v,
        Err(e) => return error(actix_web::http::StatusCode::BAD_REQUEST, &e.to_string()),
    };
    match reserve(&pool, &id, body["deadline"].as_str().unwrap(), Some(&body)).await {
        Ok(true) => {}
        Ok(false) => {
            return error(
                actix_web::http::StatusCode::CONFLICT,
                "Payment already started. Refresh to resume.",
            )
        }
        Err(_) => {
            return error(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save payment",
            )
        }
    }
    let mut setup_stage = "provider_quote";
    let result = async {
        let response = g.quote(&body).await?;
        setup_stage = "quote_validation";
        let quote = checked_quote(&response, &body, &asset, false)?;
        setup_stage = "persist_quote";
        let payload = crate::crypto::encrypt(&quote.to_string(), &config.encryption_key)?;
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE crosschain_attempts SET state='PENDING_DEPOSIT',payload=?,last_success=? WHERE invoice_id=?")
            .bind(payload).bind(Utc::now().timestamp()).bind(&id).execute(&mut *tx).await?;
        fees::bind_quote(&mut tx, &id, &response, &body).await?;
        tx.commit().await?;
        anyhow::Ok(json!({"state":"PENDING_DEPOSIT", "quote":quote, "stale":false, "canManage":true, "settlementVerified":false}))
    }.await;
    match result {
        Ok(a) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(a),
        Err(_) => {
            tracing::warn!(invoice_id = %id, stage = setup_stage, "Cross-chain payment setup failed; reservation preserved");
            // Outcome may be ambiguous: keep the reservation rather than risk a
            // second swap. No deposit instructions have been shown to the buyer.
            let _ = sqlx::query("UPDATE crosschain_attempts SET state='NEEDS_REVIEW' WHERE invoice_id=? AND payload IS NULL")
                .bind(&id).execute(pool.get_ref()).await;
            error(
                actix_web::http::StatusCode::BAD_GATEWAY,
                "Payment setup could not be verified. Refresh for recovery instructions.",
            )
        }
    }
}

async fn read_attempt(pool: &SqlitePool, id: &str, key: &str) -> anyhow::Result<Option<Value>> {
    let row: Option<(String, Option<String>, i64, Option<String>, Option<String>, bool)> = sqlx::query_as(
        "SELECT state,payload,last_success,send_token,sent_txid,
          EXISTS(SELECT 1 FROM crosschain_settlements s JOIN invoices i ON i.id=s.invoice_id WHERE s.invoice_id=a.invoice_id AND i.status='confirmed')
         FROM crosschain_attempts a WHERE invoice_id=?")
        .bind(id).fetch_optional(pool).await?;
    row.map(|(state, payload, checked, send_token, sent_txid, settlement_verified)| {
        let quote = match payload {
            Some(p) => serde_json::from_str::<Value>(&crate::crypto::decrypt(&p, key)?)?,
            None => Value::Null,
        };
        Ok(
            json!({"state":state, "quote":quote, "stale":Utc::now().timestamp()-checked > 60,
            "sendStarted": send_token.is_some(), "sentTxid": sent_txid, "settlementVerified":settlement_verified}),
        )
    })
    .transpose()
}

pub async fn status(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    g: web::Data<Gateway>,
    config: web::Data<crate::config::Config>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    let mut attempt = match read_attempt(&pool, &id, &config.encryption_key).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(json!({"state":"NONE","enabled":g.enabled && operations::enabled(&pool,&id).await.unwrap_or(false),"settlementVerified":false}))
        }
        Err(_) => {
            return error(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore payment",
            )
        }
    };
    attempt["canManage"] = json!(security::authorized(&req, &pool, &id).await);
    if attempt["canManage"] == false {
        // Public receipt viewers may see the asset, never the buyer's addresses,
        // source transaction, funding instructions or wallet-send state.
        return HttpResponse::Ok().insert_header(("Cache-Control", "no-store"))
            .json(json!({"state":attempt["state"],"stale":attempt["stale"],"canManage":false,"asset":attempt["quote"]["asset"],"settlementVerified":attempt["settlementVerified"]}));
    }
    let state = attempt["state"].as_str().unwrap_or("");
    if matches!(state, "SUCCESS" | "REFUNDED") || attempt["quote"].is_null() {
        return HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(attempt);
    }
    let now = Utc::now().timestamp();
    // Database lease bounds polling across browsers and API workers.
    let lease = sqlx::query(
        "UPDATE crosschain_attempts SET last_checked=? WHERE invoice_id=? AND last_checked < ?",
    )
    .bind(now)
    .bind(&id)
    .bind(now - 30)
    .execute(pool.get_ref())
    .await;
    if !matches!(lease, Ok(r) if r.rows_affected() == 1) {
        return HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(attempt);
    }
    let q = &attempt["quote"];
    let mut req = g
        .request(reqwest::Method::GET, "/status")
        .query(&[("depositAddress", q["depositAddress"].as_str().unwrap_or(""))]);
    if let Some(memo) = q["depositMemo"].as_str().filter(|s| !s.is_empty()) {
        req = req.query(&[("depositMemo", memo)]);
    }
    let result = async {
        let data: Value = req.send().await?.error_for_status()?.json().await?;
        signatures::verify_status(&data["quoteResponse"])?;
        let inv = crate::invoices::get_invoice(&pool, &id).await?.context("Missing invoice")?;
        let correlation: Option<String> = sqlx::query_scalar("SELECT correlation_id FROM crosschain_fees WHERE invoice_id=?").bind(&id).fetch_optional(pool.get_ref()).await?.flatten();
        fees::validate_route(&data, q, &inv.payment_address, correlation.as_deref())?;
        let state = data["status"].as_str().context("Missing status")?;
        if !matches!(state, "PENDING_DEPOSIT" | "KNOWN_DEPOSIT_TX" | "INCOMPLETE_DEPOSIT" | "PROCESSING" | "SUCCESS" | "REFUNDED" | "FAILED") { bail!("Unknown status"); }
        sqlx::query("UPDATE crosschain_attempts SET state=?,last_success=? WHERE invoice_id=? AND state NOT IN ('SUCCESS','REFUNDED')")
            .bind(state).bind(now).bind(&id).execute(pool.get_ref()).await?;
        anyhow::Ok(state.to_string())
    }.await;
    match result {
        Ok(state) => {
            attempt["state"] = json!(state);
            attempt["stale"] = json!(false);
        }
        Err(_) => {
            let _ = sqlx::query("UPDATE crosschain_attempts SET last_checked=? WHERE invoice_id=?")
                .bind(now - 20)
                .bind(&id)
                .execute(pool.get_ref())
                .await;
            attempt["stale"] = json!(true);
        }
    }
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(attempt)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendInput {
    action: String,
    token: Option<String>,
    txid: Option<String>,
}

// A durable send lock protects against double clicks, reloads and another tab.
// Only a definite wallet rejection may release it. Ambiguous RPC errors keep it.
pub async fn wallet_send(
    req: actix_web::HttpRequest,
    pool: web::Data<SqlitePool>,
    config: web::Data<crate::config::Config>,
    path: web::Path<String>,
    body: web::Json<SendInput>,
) -> HttpResponse {
    let id = path.into_inner();
    if !security::authorized(&req, &pool, &id).await {
        return error(
            actix_web::http::StatusCode::FORBIDDEN,
            "Open the full payment link provided by the merchant to start or resume this payment.",
        );
    }
    let result = async {
        match body.action.as_str() {
            "start" => {
                let a = read_attempt(&pool, &id, &config.encryption_key).await?.context("No payment attempt")?;
                ensure!(a["state"] == "PENDING_DEPOSIT", "Payment already in progress");
                ensure!(a["stale"] == false, "Payment status is unavailable. Wait for it to refresh before sending.");
                let deadline = a["quote"]["deadline"].as_str().context("No payment instructions")?;
                ensure!(chrono::DateTime::parse_from_rfc3339(deadline)? > Utc::now(), "Quote expired");
                let token = uuid::Uuid::new_v4().to_string();
                let rows = sqlx::query("UPDATE crosschain_attempts SET send_token=? WHERE invoice_id=? AND send_token IS NULL AND state='PENDING_DEPOSIT'
                    AND EXISTS (SELECT 1 FROM invoices WHERE id=? AND status='pending' AND received_zatoshis=0)")
                    .bind(&token).bind(&id).bind(&id).execute(pool.get_ref()).await?.rows_affected();
                ensure!(rows == 1, "Payment already started. Refresh to resume.");
                Ok(json!({"token":token}))
            },
            "rejected" | "submitted" => {
                let token = body.token.as_deref().context("Missing send token")?;
                ensure!(uuid::Uuid::parse_str(token).is_ok(), "Invalid send token");
                let rows = if body.action == "rejected" {
                    sqlx::query("UPDATE crosschain_attempts SET send_token=NULL WHERE invoice_id=? AND send_token=? AND sent_txid IS NULL AND state='PENDING_DEPOSIT'")
                        .bind(&id).bind(token).execute(pool.get_ref()).await?.rows_affected()
                } else {
                    let txid = body.txid.as_deref().context("Missing transaction id")?;
                    ensure!((32..=128).contains(&txid.len()) && txid.bytes().all(|b| b.is_ascii_alphanumeric()), "Invalid transaction id");
                    sqlx::query("UPDATE crosschain_attempts SET sent_txid=? WHERE invoice_id=? AND send_token=? AND sent_txid IS NULL")
                        .bind(txid).bind(&id).bind(token).execute(pool.get_ref()).await?.rows_affected()
                };
                ensure!(rows == 1, "Payment state changed. Refresh to resume.");
                Ok(json!({"ok":true}))
            },
            _ => bail!("Invalid action"),
        }
    }.await;
    match result {
        Ok(v) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(v),
        Err(e) => error(actix_web::http::StatusCode::CONFLICT, &e.to_string()),
    }
}

#[cfg(test)]
mod tests;
