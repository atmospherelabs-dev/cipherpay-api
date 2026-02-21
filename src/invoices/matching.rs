use super::Invoice;

/// Finds a pending invoice whose memo_code matches the decrypted memo text.
pub fn find_matching_invoice<'a>(
    invoices: &'a [Invoice],
    memo_text: &str,
) -> Option<&'a Invoice> {
    let memo_trimmed = memo_text.trim();

    // Exact match first
    if let Some(inv) = invoices.iter().find(|i| i.memo_code == memo_trimmed) {
        return Some(inv);
    }

    // Contains match (memo field may have padding or extra text)
    invoices.iter().find(|i| memo_trimmed.contains(&i.memo_code))
}
