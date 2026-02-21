use anyhow::Result;
use std::io::Cursor;

use zcash_note_encryption::try_note_decryption;
use orchard::{
    keys::{FullViewingKey, Scope, PreparedIncomingViewingKey},
    note_encryption::OrchardDomain,
};
use zcash_address::unified::{Container, Encoding, Fvk, Ufvk};
use zcash_primitives::transaction::Transaction;

/// Accept payments within 0.5% of invoice price to account for
/// wallet rounding and network fee differences.
pub const SLIPPAGE_TOLERANCE: f64 = 0.995;

pub struct DecryptedOutput {
    pub memo: String,
    pub amount_zec: f64,
}

/// Parse a UFVK string and extract the Orchard FullViewingKey.
fn parse_orchard_fvk(ufvk_str: &str) -> Result<FullViewingKey> {
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

/// Trial-decrypt all Orchard outputs in a raw transaction hex using the
/// provided UFVK. Returns the first successfully decrypted output with
/// its memo text and amount.
pub fn try_decrypt_outputs(raw_hex: &str, ufvk_str: &str) -> Result<Option<DecryptedOutput>> {
    let tx_bytes = hex::decode(raw_hex)?;
    if tx_bytes.len() < 4 {
        return Ok(None);
    }

    let fvk = match parse_orchard_fvk(ufvk_str) {
        Ok(fvk) => fvk,
        Err(e) => {
            tracing::debug!(error = %e, "UFVK parsing failed");
            return Ok(None);
        }
    };

    let mut cursor = Cursor::new(&tx_bytes[..]);
    let tx = match Transaction::read(&mut cursor, zcash_primitives::consensus::BranchId::Nu5) {
        Ok(tx) => tx,
        Err(_) => return Ok(None),
    };

    let bundle = match tx.orchard_bundle() {
        Some(b) => b,
        None => return Ok(None),
    };

    let actions: Vec<_> = bundle.actions().iter().collect();

    for action in &actions {
        let domain = OrchardDomain::for_action(*action);

        for scope in [Scope::External, Scope::Internal] {
            let ivk = fvk.to_ivk(scope);
            let prepared_ivk = PreparedIncomingViewingKey::new(&ivk);

            if let Some((note, _recipient, memo)) = try_note_decryption(&domain, &prepared_ivk, *action) {
                let memo_bytes = memo.as_slice();
                let memo_len = memo_bytes.iter()
                    .position(|&b| b == 0)
                    .unwrap_or(memo_bytes.len());

                if memo_len == 0 {
                    continue;
                }

                if let Ok(memo_text) = String::from_utf8(memo_bytes[..memo_len].to_vec()) {
                    if !memo_text.trim().is_empty() {
                        let amount_zatoshis = note.value().inner();
                        let amount_zec = amount_zatoshis as f64 / 100_000_000.0;

                        tracing::info!(
                            memo = %memo_text,
                            amount_zec,
                            "Decrypted Orchard output"
                        );

                        return Ok(Some(DecryptedOutput {
                            memo: memo_text,
                            amount_zec,
                        }));
                    }
                }
            }
        }
    }

    Ok(None)
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
