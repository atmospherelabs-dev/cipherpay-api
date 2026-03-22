use anyhow::Result;
use std::io::Cursor;

use zcash_note_encryption::try_note_decryption;
use orchard::{
    keys::{FullViewingKey, IncomingViewingKey, Scope, PreparedIncomingViewingKey},
    note_encryption::OrchardDomain,
};
use zcash_address::unified::{Container, Encoding, Fvk, Ivk, Ufvk, Uivk};
#[allow(deprecated)]
use zcash_address::Network as NetworkType;
use zcash_primitives::transaction::Transaction;

/// Accept payments within 0.5% of invoice price to account for
/// wallet rounding and network fee differences.
pub const SLIPPAGE_TOLERANCE: f64 = 0.995;

/// Minimum payment as a fraction of invoice price to accept as underpaid
/// and extend expiry. Prevents dust-spam attacks that keep invoices alive.
pub const DUST_THRESHOLD_FRACTION: f64 = 0.01; // 1% of invoice price
pub const DUST_THRESHOLD_MIN_ZATOSHIS: i64 = 10_000; // 0.0001 ZEC absolute floor

pub struct DecryptedOutput {
    pub memo: String,
    pub amount_zec: f64,
    pub amount_zatoshis: u64,
    pub recipient_raw: [u8; 43],
}

/// Pre-computed key for a merchant. Only External scope is needed
/// since CipherPay only detects incoming payments (not change outputs).
pub struct CachedKeys {
    pub pivk_external: PreparedIncomingViewingKey,
}

/// Prepare cached keys from a viewing key string (UIVK or legacy UFVK).
/// Call once per merchant, reuse across scans.
pub fn prepare_keys(key_str: &str) -> Result<CachedKeys> {
    let ivk = parse_orchard_ivk(key_str)?;
    let pivk_external = PreparedIncomingViewingKey::new(&ivk);
    Ok(CachedKeys { pivk_external })
}

/// Parse an Orchard IncomingViewingKey from either a UIVK or UFVK string.
pub fn parse_orchard_ivk(key_str: &str) -> Result<IncomingViewingKey> {
    if key_str.starts_with("uivk") || key_str.starts_with("uivktest") {
        parse_ivk_from_uivk(key_str)
    } else {
        let fvk = parse_orchard_fvk(key_str)?;
        Ok(fvk.to_ivk(Scope::External))
    }
}

/// Parse an Orchard IVK from a viewing key string, also returning the network.
/// Used for address derivation where we need the network for UA encoding.
pub fn parse_key_with_network(key_str: &str) -> Result<(NetworkType, IncomingViewingKey)> {
    if key_str.starts_with("uivk") || key_str.starts_with("uivktest") {
        let (network, uivk) = Uivk::decode(key_str)
            .map_err(|e| anyhow::anyhow!("UIVK decode failed: {:?}", e))?;
        let ivk_bytes = uivk.items().iter().find_map(|ivk| {
            match ivk {
                Ivk::Orchard(data) => Some(*data),
                _ => None,
            }
        }).ok_or_else(|| anyhow::anyhow!("No Orchard IVK found in UIVK"))?;
        let ivk = IncomingViewingKey::from_bytes(&ivk_bytes)
            .into_option()
            .ok_or_else(|| anyhow::anyhow!("Failed to parse Orchard IVK from bytes"))?;
        Ok((network, ivk))
    } else {
        let (network, _) = Ufvk::decode(key_str)
            .map_err(|e| anyhow::anyhow!("UFVK decode failed: {:?}", e))?;
        let fvk = parse_orchard_fvk(key_str)?;
        Ok((network, fvk.to_ivk(Scope::External)))
    }
}

/// Parse an Orchard IVK directly from a UIVK string.
fn parse_ivk_from_uivk(uivk_str: &str) -> Result<IncomingViewingKey> {
    let (_network, uivk) = Uivk::decode(uivk_str)
        .map_err(|e| anyhow::anyhow!("UIVK decode failed: {:?}", e))?;

    let ivk_bytes = uivk.items().iter().find_map(|ivk| {
        match ivk {
            Ivk::Orchard(data) => Some(*data),
            _ => None,
        }
    }).ok_or_else(|| anyhow::anyhow!("No Orchard IVK found in UIVK"))?;

    IncomingViewingKey::from_bytes(&ivk_bytes)
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Orchard IVK from bytes"))
}

/// Derive a UIVK string from a UFVK string. Extracts the External IVK
/// and encodes it as a proper UIVK (ZIP 316).
pub fn derive_uivk_from_ufvk(ufvk_str: &str) -> Result<String> {
    let (network, _) = Ufvk::decode(ufvk_str)
        .map_err(|e| anyhow::anyhow!("UFVK decode failed: {:?}", e))?;

    let fvk = parse_orchard_fvk(ufvk_str)?;
    let ivk = fvk.to_ivk(Scope::External);
    let ivk_bytes = ivk.to_bytes();

    let uivk = Uivk::try_from_items(vec![Ivk::Orchard(ivk_bytes)])
        .map_err(|e| anyhow::anyhow!("UIVK construction failed: {:?}", e))?;

    Ok(uivk.encode(&network))
}

/// Trial-decrypt all Orchard outputs using pre-computed keys (fast path).
pub fn try_decrypt_with_keys(raw_hex: &str, keys: &CachedKeys) -> Result<Vec<DecryptedOutput>> {
    let tx_bytes = hex::decode(raw_hex)?;
    if tx_bytes.len() < 4 {
        return Ok(vec![]);
    }

    let mut cursor = Cursor::new(&tx_bytes[..]);
    let tx = match Transaction::read(&mut cursor, zcash_primitives::consensus::BranchId::Nu5) {
        Ok(tx) => tx,
        Err(_) => return Ok(vec![]),
    };

    let bundle = match tx.orchard_bundle() {
        Some(b) => b,
        None => return Ok(vec![]),
    };

    let actions: Vec<_> = bundle.actions().iter().collect();
    let mut outputs = Vec::new();

    for action in &actions {
        let domain = OrchardDomain::for_action(*action);

        {
            let pivk = &keys.pivk_external;
            if let Some((note, _recipient, memo)) = try_note_decryption(&domain, pivk, *action) {
                let recipient_raw = note.recipient().to_raw_address_bytes();
                let memo_bytes = memo.as_slice();
                let memo_len = memo_bytes.iter()
                    .position(|&b| b == 0)
                    .unwrap_or(memo_bytes.len());

                let memo_text = if memo_len > 0 {
                    String::from_utf8(memo_bytes[..memo_len].to_vec())
                        .unwrap_or_default()
                } else {
                    String::new()
                };

                let amount_zatoshis = note.value().inner();
                let amount_zec = amount_zatoshis as f64 / 100_000_000.0;

                if !memo_text.trim().is_empty() {
                    tracing::info!(
                        has_memo = true,
                        memo_len = memo_text.len(),
                        amount_zec,
                        "Decrypted Orchard output"
                    );
                }

                outputs.push(DecryptedOutput {
                    memo: memo_text,
                    amount_zec,
                    amount_zatoshis,
                    recipient_raw,
                });
            }
        }
    }

    Ok(outputs)
}

/// Parse a UFVK string and extract the Orchard FullViewingKey.
pub(crate) fn parse_orchard_fvk(ufvk_str: &str) -> Result<FullViewingKey> {
    let (_network, ufvk) = Ufvk::decode(ufvk_str)
        .map_err(|e| anyhow::anyhow!("UFVK decode failed: {:?}", e))?;

    let orchard_fvk_bytes = ufvk.items().iter().find_map(|fvk| {
        match fvk {
            Fvk::Orchard(data) => Some(data.clone()),
            _ => None,
        }
    }).ok_or_else(|| anyhow::anyhow!("No Orchard FVK found in UFVK"))?;

    FullViewingKey::from_bytes(&orchard_fvk_bytes)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Orchard FVK from bytes"))
}

/// Trial-decrypt all Orchard outputs using a viewing key (UIVK or UFVK).
/// Returns the first successfully decrypted output.
pub fn try_decrypt_outputs(raw_hex: &str, key_str: &str) -> Result<Option<DecryptedOutput>> {
    let results = try_decrypt_all_outputs_ivk(raw_hex, key_str)?;
    Ok(results.into_iter().next())
}

/// Trial-decrypt ALL Orchard outputs using a viewing key (UIVK or UFVK).
/// External scope only -- sufficient for incoming payment detection.
pub fn try_decrypt_all_outputs_ivk(raw_hex: &str, key_str: &str) -> Result<Vec<DecryptedOutput>> {
    let tx_bytes = hex::decode(raw_hex)?;
    if tx_bytes.len() < 4 {
        return Ok(vec![]);
    }

    let ivk = match parse_orchard_ivk(key_str) {
        Ok(ivk) => ivk,
        Err(e) => {
            tracing::debug!(error = %e, "Viewing key parsing failed");
            return Ok(vec![]);
        }
    };

    let prepared_ivk = PreparedIncomingViewingKey::new(&ivk);

    let mut cursor = Cursor::new(&tx_bytes[..]);
    let tx = match Transaction::read(&mut cursor, zcash_primitives::consensus::BranchId::Nu5) {
        Ok(tx) => tx,
        Err(_) => return Ok(vec![]),
    };

    let bundle = match tx.orchard_bundle() {
        Some(b) => b,
        None => return Ok(vec![]),
    };

    let actions: Vec<_> = bundle.actions().iter().collect();
    let mut outputs = Vec::new();

    for action in &actions {
        let domain = OrchardDomain::for_action(*action);

        if let Some((note, _recipient, memo)) = try_note_decryption(&domain, &prepared_ivk, *action) {
            let recipient_raw = note.recipient().to_raw_address_bytes();
            let memo_bytes = memo.as_slice();
            let memo_len = memo_bytes.iter()
                .position(|&b| b == 0)
                .unwrap_or(memo_bytes.len());

            let memo_text = if memo_len > 0 {
                String::from_utf8(memo_bytes[..memo_len].to_vec())
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let amount_zatoshis = note.value().inner();
            let amount_zec = amount_zatoshis as f64 / 100_000_000.0;

            if !memo_text.trim().is_empty() {
                tracing::info!(
                    has_memo = true,
                    memo_len = memo_text.len(),
                    amount_zec,
                    "Decrypted Orchard output"
                );
            }

            outputs.push(DecryptedOutput {
                memo: memo_text,
                amount_zec,
                amount_zatoshis,
                recipient_raw,
            });
        }
    }

    Ok(outputs)
}

/// Trial-decrypt ALL Orchard outputs using a UFVK (both scopes).
/// Used for fee detection where CipherPay's own FEE_UFVK needs full scope scanning.
pub fn try_decrypt_all_outputs(raw_hex: &str, ufvk_str: &str) -> Result<Vec<DecryptedOutput>> {
    let tx_bytes = hex::decode(raw_hex)?;
    if tx_bytes.len() < 4 {
        return Ok(vec![]);
    }

    let fvk = match parse_orchard_fvk(ufvk_str) {
        Ok(fvk) => fvk,
        Err(e) => {
            tracing::debug!(error = %e, "UFVK parsing failed");
            return Ok(vec![]);
        }
    };

    let mut cursor = Cursor::new(&tx_bytes[..]);
    let tx = match Transaction::read(&mut cursor, zcash_primitives::consensus::BranchId::Nu5) {
        Ok(tx) => tx,
        Err(_) => return Ok(vec![]),
    };

    let bundle = match tx.orchard_bundle() {
        Some(b) => b,
        None => return Ok(vec![]),
    };

    let actions: Vec<_> = bundle.actions().iter().collect();
    let mut outputs = Vec::new();

    for action in &actions {
        let domain = OrchardDomain::for_action(*action);

        for scope in [Scope::External, Scope::Internal] {
            let ivk = fvk.to_ivk(scope);
            let prepared_ivk = PreparedIncomingViewingKey::new(&ivk);

            if let Some((note, _recipient, memo)) = try_note_decryption(&domain, &prepared_ivk, *action) {
                let recipient_raw = note.recipient().to_raw_address_bytes();
                let memo_bytes = memo.as_slice();
                let memo_len = memo_bytes.iter()
                    .position(|&b| b == 0)
                    .unwrap_or(memo_bytes.len());

                let memo_text = if memo_len > 0 {
                    String::from_utf8(memo_bytes[..memo_len].to_vec())
                        .unwrap_or_default()
                } else {
                    String::new()
                };

                let amount_zatoshis = note.value().inner();
                let amount_zec = amount_zatoshis as f64 / 100_000_000.0;

                if !memo_text.trim().is_empty() {
                    tracing::info!(
                        has_memo = true,
                        memo_len = memo_text.len(),
                        amount_zec,
                        "Decrypted Orchard output"
                    );
                }

                outputs.push(DecryptedOutput {
                    memo: memo_text,
                    amount_zec,
                    amount_zatoshis,
                    recipient_raw,
                });
            }
        }
    }

    Ok(outputs)
}

/// Returns just the memo string (convenience wrapper).
#[allow(dead_code)]
pub fn try_decrypt_memo(raw_hex: &str, ufvk: &str) -> Result<Option<String>> {
    match try_decrypt_outputs(raw_hex, ufvk)? {
        Some(output) => Ok(Some(output.memo)),
        None => Ok(None),
    }
}

/// Extracts memo text from raw memo bytes (512 bytes in Zcash).
#[allow(dead_code)]
fn memo_bytes_to_text(memo_bytes: &[u8]) -> Option<String> {
    if memo_bytes.is_empty() {
        return None;
    }

    match memo_bytes[0] {
        0xF6 => None,
        0xFF => None,
        _ => {
            let end = memo_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(memo_bytes.len());

            String::from_utf8(memo_bytes[..end].to_vec()).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memo_bytes_to_text() {
        let mut memo = vec![0u8; 512];
        let text = b"CP-A7F3B2C1";
        memo[..text.len()].copy_from_slice(text);
        assert_eq!(memo_bytes_to_text(&memo), Some("CP-A7F3B2C1".to_string()));

        let memo = vec![0xF6; 512];
        assert_eq!(memo_bytes_to_text(&memo), None);

        let mut memo = vec![0u8; 512];
        memo[0] = 0xFF;
        assert_eq!(memo_bytes_to_text(&memo), None);
    }

    #[test]
    fn test_try_decrypt_stub_returns_none() {
        let result = try_decrypt_memo("deadbeef", "uviewtest1dummy").unwrap();
        assert_eq!(result, None);
    }
}
