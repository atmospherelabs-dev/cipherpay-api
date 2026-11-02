use super::Invoice;
use std::collections::HashMap;
use zcash_address::unified::{Container, Encoding, Receiver};

/// Both the decrypting merchant and exact receiver are part of the payment identity.
pub struct InvoiceIndex<'a> {
    by_address: HashMap<(&'a str, &'a str), &'a Invoice>,
    legacy: Vec<(&'a Invoice, String)>,
}

pub fn orchard_receiver(address: &str) -> Option<String> {
    let (_, ua) = zcash_address::unified::Address::decode(address).ok()?;
    ua.items().iter().find_map(|r| match r {
        Receiver::Orchard(raw) => Some(hex::encode(raw)),
        _ => None,
    })
}

impl<'a> InvoiceIndex<'a> {
    pub fn build(invoices: &'a [Invoice]) -> Self {
        let mut by_address = HashMap::with_capacity(invoices.len());
        let mut legacy = Vec::new();
        for inv in invoices {
            if let Some(ref addr) = inv.orchard_receiver_hex {
                by_address.insert((inv.merchant_id.as_str(), addr.as_str()), inv);
            } else if let Some(receiver) = orchard_receiver(&inv.payment_address) {
                legacy.push((inv, receiver));
            }
        }
        Self { by_address, legacy }
    }

    pub fn find(
        &self,
        merchant_id: &str,
        recipient_hex: &str,
        memo_text: &str,
    ) -> Option<&'a Invoice> {
        if let Some(inv) = self.by_address.get(&(merchant_id, recipient_hex)) {
            return Some(inv);
        }
        // Legacy shared addresses require an exact memo, never substring matching.
        let memo = memo_text.trim();
        self.legacy.iter().find_map(|(inv, receiver)| {
            (inv.merchant_id == merchant_id
                && receiver == recipient_hex
                && !memo.is_empty()
                && inv.memo_code == memo)
                .then_some(*inv)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invoice(merchant: &str, receiver: Option<&str>) -> Invoice {
        serde_json::from_value(serde_json::json!({
            "id":"invoice", "merchant_id":merchant, "memo_code":"CP-12345678",
            "price_eur":1.0,"price_zec":0.01,"zec_rate_at_creation":100.0,
            "payment_address":"invalid", "zcash_uri":"", "status":"pending",
            "expires_at":"", "created_at":"", "price_zatoshis":1000000,
            "received_zatoshis":0,"is_donation":0,"campaign_counted":0
        }))
        .map(|mut i: Invoice| {
            i.orchard_receiver_hex = receiver.map(str::to_string);
            i
        })
        .unwrap()
    }
    #[test]
    fn diversified_invoice_requires_merchant_and_receiver() {
        let invoices = vec![invoice("merchant-a", Some("receiver-a"))];
        let index = InvoiceIndex::build(&invoices);
        assert!(index
            .find("merchant-b", "receiver-b", "CP-12345678")
            .is_none());
        assert!(index
            .find("merchant-a", "receiver-b", "CP-12345678")
            .is_none());
        assert!(index
            .find("merchant-b", "receiver-a", "CP-12345678")
            .is_none());
        assert!(index
            .find("merchant-a", "receiver-a", "unrelated")
            .is_some());
    }
    #[test]
    fn legacy_requires_exact_memo_and_valid_receiver() {
        let raw = [7u8; 43];
        let ua =
            zcash_address::unified::Address::try_from_items(vec![Receiver::Orchard(raw)]).unwrap();
        let mut inv = invoice("a", None);
        inv.payment_address = ua.encode(&zcash_protocol::consensus::NetworkType::Main);
        let invoices = vec![inv];
        let index = InvoiceIndex::build(&invoices);
        let receiver = hex::encode(raw);
        assert!(index.find("a", &receiver, "CP-12345678").is_some());
        assert!(index.find("b", &receiver, "CP-12345678").is_none());
        assert!(index.find("a", "wrong", "CP-12345678").is_none());
        assert!(index.find("a", &receiver, "prefix CP-12345678").is_none());
    }
}
