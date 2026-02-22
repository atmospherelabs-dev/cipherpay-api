use super::Invoice;

/// Primary matching: find an invoice by its Orchard receiver address.
/// The cryptographic address is the authoritative source of truth.
pub fn find_by_address<'a>(
    invoices: &'a [Invoice],
    recipient_hex: &str,
) -> Option<&'a Invoice> {
    invoices.iter().find(|i| {
        i.orchard_receiver_hex.as_deref() == Some(recipient_hex)
    })
}

/// Fallback matching: find a pending invoice whose memo_code matches the decrypted memo text.
/// Only used for old invoices created before diversified addresses were enabled.
pub fn find_by_memo<'a>(
    invoices: &'a [Invoice],
    memo_text: &str,
) -> Option<&'a Invoice> {
    let memo_trimmed = memo_text.trim();
    if memo_trimmed.is_empty() {
        return None;
    }

    if let Some(inv) = invoices.iter().find(|i| i.memo_code == memo_trimmed) {
        return Some(inv);
    }

    invoices.iter().find(|i| memo_trimmed.contains(&i.memo_code))
}

/// Find the matching invoice using address-first, memo-fallback strategy.
/// Security invariant: if address matches Invoice A, that wins unconditionally,
/// even if the memo points to a different invoice.
pub fn find_matching_invoice<'a>(
    invoices: &'a [Invoice],
    recipient_hex: &str,
    memo_text: &str,
) -> Option<&'a Invoice> {
    if let Some(inv) = find_by_address(invoices, recipient_hex) {
        return Some(inv);
    }

    find_by_memo(invoices, memo_text)
}
