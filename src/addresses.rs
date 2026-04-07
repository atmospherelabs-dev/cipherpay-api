use anyhow::Result;
use zcash_address::unified::{Encoding, Receiver};

pub struct DerivedAddress {
    pub ua_string: String,
    pub orchard_receiver_hex: String,
}

/// Derive a unique Orchard payment address from a viewing key (UIVK or UFVK)
/// at the given diversifier index.
/// Returns both the Unified Address string (for QR/display) and the raw receiver hex (for DB lookup).
pub fn derive_invoice_address(key_str: &str, index: u32) -> Result<DerivedAddress> {
    let (network, ivk) = crate::scanner::decrypt::parse_key_with_network(key_str)?;
    let addr = ivk.address_at(index);
    let raw = addr.to_raw_address_bytes();
    let orchard_receiver_hex = hex::encode(raw);

    let ua = zcash_address::unified::Address::try_from_items(vec![Receiver::Orchard(raw)])
        .map_err(|e| anyhow::anyhow!("UA construction failed: {:?}", e))?;

    let ua_string = ua.encode(&network);

    tracing::debug!(
        diversifier_index = index,
        "Derived invoice address from viewing key"
    );

    Ok(DerivedAddress {
        ua_string,
        orchard_receiver_hex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_different_indices_produce_different_addresses() {
        let test_key = std::env::var("TEST_UFVK").unwrap_or_default();
        if test_key.is_empty() {
            return;
        }

        let addr0 = derive_invoice_address(&test_key, 0).unwrap();
        let addr1 = derive_invoice_address(&test_key, 1).unwrap();

        assert_ne!(addr0.ua_string, addr1.ua_string);
        assert_ne!(addr0.orchard_receiver_hex, addr1.orchard_receiver_hex);
    }
}
