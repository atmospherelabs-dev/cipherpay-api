//! Matches @defuse-protocol/one-click-sdk-typescript 0.1.26 signed field selection.
//! Fees and confidentiality are not signed by 1Click; validate them separately
//! against the authenticated response rather than treating them as signed proof.
use anyhow::{ensure, Context};
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
const MANAGER: &str = "reYaWhvwu8Jzo3WUM3zhn6VrhuMEF4eADL17qtRVifc";

fn truthy(v: &Value) -> bool {
    !v.is_null() && v != false && v != 0 && v != ""
}
fn message(response: &Value) -> anyhow::Result<String> {
    let req = response["quoteRequest"].as_object().context("Missing signed request")?;
    let quote = response["quote"].as_object().context("Missing signed quote")?;
    let mut signed = Map::new();
    for k in ["dry", "swapType", "slippageTolerance", "originAsset", "depositType", "destinationAsset", "amount", "refundTo", "refundType", "recipient", "recipientType", "deadline"] {
        if let Some(v) = req.get(k) { signed.insert(k.into(), v.clone()); }
    }
    for k in ["quoteWaitingTimeMs", "referral", "virtualChainRecipient", "virtualChainRefundRecipient", "customRecipientMsg"] {
        if let Some(v) = req.get(k).filter(|v| truthy(v)) { signed.insert(k.into(), v.clone()); }
    }
    for k in ["amountIn", "amountInFormatted", "amountInUsd", "minAmountIn", "amountOut", "amountOutFormatted", "amountOutUsd", "minAmountOut"] {
        if let Some(v) = quote.get(k) { signed.insert(k.into(), v.clone()); }
    }
    if req.get("dry") != Some(&Value::Bool(true)) {
        // Object spread in the SDK overrides the request's deadline, even if absent.
        signed.remove("deadline");
        for k in ["depositAddress", "depositMemo", "deadline", "timeWhenInactive", "timeEstimate", "refundFee", "withdrawFee"] {
            if let Some(v) = quote.get(k).filter(|v| truthy(v)) { signed.insert(k.into(), v.clone()); }
        }
    }
    signed.insert("timestamp".into(), response.get("timestamp").context("Missing signature timestamp")?.clone());
    // RFC 8785 uses ECMAScript number formatting, including integral floats.
    let bytes = serde_json_canonicalizer::to_vec(&Value::Object(signed))?;
    Ok(bs58::encode(Sha256::digest(bytes)).into_string())
}
fn verify_with(response: &Value, key: &[u8;32]) -> anyhow::Result<()> {
    let raw = response["signature"].as_str().context("Missing quote signature")?;
    let bytes = bs58::decode(raw.strip_prefix("ed25519:").unwrap_or(raw)).into_vec()?;
    let sig = Signature::from_slice(&bytes)?;
    VerifyingKey::from_bytes(key)?.verify_strict(message(response)?.as_bytes(), &sig)?;
    Ok(())
}
pub(super) fn verify(response: &Value) -> anyhow::Result<()> {
    let key: [u8;32] = bs58::decode(MANAGER).into_vec()?.try_into().map_err(|_| anyhow::anyhow!("Invalid pinned signer"))?;
    #[cfg(test)]
    if verify_with(response, &ed25519_dalek::SigningKey::from_bytes(&[42;32]).verifying_key().to_bytes()).is_ok() { return Ok(()); }
    verify_with(response, &key)
}
pub(super) fn verify_fresh(response: &Value) -> anyhow::Result<()> {
    verify(response)?;
    let timestamp = response["timestamp"].as_str().context("Missing quote timestamp")?;
    let age = chrono::Utc::now().signed_duration_since(chrono::DateTime::parse_from_rfc3339(timestamp)?).num_seconds();
    ensure!((-60..=300).contains(&age), "Stale quote signature");
    Ok(())
}
/// 1Click status omits the default quoteWaitingTimeMs even though the original
/// signature covers it. Confirmed against both fresh and historical real swaps.
/// Restore only this known default, and still require the pinned signature.
pub(super) fn verify_status(response: &Value) -> anyhow::Result<()> {
    if verify(response).is_ok() { return Ok(()); }
    ensure!(response["quoteRequest"].is_object(), "Missing signed request");
    ensure!(response["quoteRequest"].get("quoteWaitingTimeMs").is_none(), "Invalid status signature");
    let mut normalized = response.clone();
    normalized["quoteRequest"]["quoteWaitingTimeMs"] = Value::from(3000);
    verify(&normalized)
}
#[cfg(test)]
pub(super) fn signed(mut response: Value) -> Value {
    use ed25519_dalek::Signer;
    response["timestamp"] = Value::String(chrono::Utc::now().to_rfc3339());
    let sig = ed25519_dalek::SigningKey::from_bytes(&[42;32]).sign(message(&response).unwrap().as_bytes());
    response["signature"] = Value::String(format!("ed25519:{}",bs58::encode(sig.to_bytes()).into_string()));
    response
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn status_normalization_preserves_signature_and_tamper_checks() {
        let original=signed(json!({"quoteRequest":{"dry":false,"quoteWaitingTimeMs":3000,"recipient":"merchant"},"quote":{"depositAddress":"deposit","amountOut":"123"}}));
        let mut status=original.clone();
        status["quoteRequest"].as_object_mut().unwrap().remove("quoteWaitingTimeMs");
        assert!(verify(&status).is_err());
        verify_status(&status).unwrap();
        status["quote"]["depositAddress"]=json!("attacker");
        assert!(verify_status(&status).is_err());
        let mut altered=original;
        altered["quoteRequest"]["quoteWaitingTimeMs"]=json!(4000);
        assert!(verify_status(&altered).is_err());
        let mut other=signed(json!({"quoteRequest":{"dry":false,"quoteWaitingTimeMs":4000},"quote":{"amountOut":"123"}}));
        other["quoteRequest"].as_object_mut().unwrap().remove("quoteWaitingTimeMs");
        assert!(verify_status(&other).is_err());
    }
    #[test]
    fn signatures_reject_mutation_missing_signer_and_stale_quotes() {
        let q=signed(json!({"quoteRequest":{"dry":false,"recipient":"merchant","deadline":"old"},"quote":{"amountIn":"123","amountOut":"456","depositAddress":"deposit","deadline":"new"}}));
        verify_fresh(&q).unwrap();
        for key in ["depositAddress","deadline","amountIn","amountOut"] {
            let mut bad=q.clone();bad["quote"][key]=json!("attacker");assert!(verify(&bad).is_err());
        }
        let mut bad=q.clone();bad["quoteRequest"]["recipient"]=json!("attacker");assert!(verify(&bad).is_err());
        let mut bad=q.clone();bad["signature"]=Value::Null;assert!(verify(&bad).is_err());
        assert!(verify_with(&q,&[1;32]).is_err());
        let mut old=q;old["timestamp"]=json!("2020-01-01T00:00:00Z");
        use ed25519_dalek::Signer;
        let sig=ed25519_dalek::SigningKey::from_bytes(&[42;32]).sign(message(&old).unwrap().as_bytes());
        old["signature"]=json!(bs58::encode(sig.to_bytes()).into_string());
        verify(&old).unwrap();assert!(verify_fresh(&old).is_err());
    }
}

#[test]
#[ignore = "requires a real provider fixture supplied via SIGNED_QUOTE_FIXTURE"]
fn real_provider_signature() {
    let file=std::env::var("SIGNED_QUOTE_FIXTURE").unwrap();
    let q:Value=serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let key:[u8;32]=bs58::decode(MANAGER).into_vec().unwrap().try_into().unwrap();
    if std::env::var("SIGNED_QUOTE_STATUS").is_ok() { verify_status(&q).unwrap(); }
    else { verify_with(&q,&key).unwrap(); }
    let mut tampered=q;tampered["quote"]["amountIn"]=Value::String("1".into());assert!(verify_with(&tampered,&key).is_err());
}
