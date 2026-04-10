use super::Invoice;
use std::collections::HashMap;

/// Pre-built index for O(1) invoice matching by Orchard receiver address.
pub struct InvoiceIndex<'a> {
    by_address: HashMap<&'a str, &'a Invoice>,
    invoices: &'a [Invoice],
}

impl<'a> InvoiceIndex<'a> {
    pub fn build(invoices: &'a [Invoice]) -> Self {
        let mut by_address = HashMap::with_capacity(invoices.len());
        for inv in invoices {
            if let Some(ref addr) = inv.orchard_receiver_hex {
                by_address.insert(addr.as_str(), inv);
            }
        }
        Self {
            by_address,
            invoices,
        }
    }

    /// O(1) address lookup, then linear memo fallback for legacy invoices.
    /// Security invariant: address match wins unconditionally over memo.
    pub fn find(&self, recipient_hex: &str, memo_text: &str) -> Option<&'a Invoice> {
        if let Some(inv) = self.by_address.get(recipient_hex) {
            return Some(inv);
        }
        find_by_memo(self.invoices, memo_text)
    }
}

/// Fallback matching: find a pending invoice whose memo_code matches the decrypted memo text.
/// Only used for old invoices created before diversified addresses were enabled.
fn find_by_memo<'a>(invoices: &'a [Invoice], memo_text: &str) -> Option<&'a Invoice> {
    let memo_trimmed = memo_text.trim();
    if memo_trimmed.is_empty() {
        return None;
    }

    if let Some(inv) = invoices.iter().find(|i| i.memo_code == memo_trimmed) {
        return Some(inv);
    }

    invoices
        .iter()
        .find(|i| memo_trimmed.contains(&i.memo_code))
}
