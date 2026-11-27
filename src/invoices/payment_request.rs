//! Machine-readable payment request for agents and wallets.
//!
//! The stored ZIP-321 URI stays the authority for what a wallet pays; this
//! module only restates it with integer zatoshis and decoded memos, and says
//! whether a direct shielded payment would be accepted right now.
use base64::Engine;
use serde::Serialize;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Output {
    pub address: String,
    pub amount_zatoshis: i64,
    pub memo: String,
    pub purpose: &'static str,
    /// The invoice output must be paid. The platform fee output is optional:
    /// when a payer omits it, the fee is billed to the merchant instead.
    pub required: bool,
}

/// Why a direct payment must not be sent. `None` means payable.
pub fn not_payable_reason(
    status: &str,
    unexpired: bool,
    received_zatoshis: i64,
    detected: bool,
    crosschain_started: bool,
) -> Option<&'static str> {
    match status {
        "pending" => {}
        "draft" => return Some("draft"),
        "expired" => return Some("expired"),
        "underpaid" => return Some("underpaid"),
        "detected" | "confirmed" => return Some("already_paid"),
        "refunded" => return Some("refunded"),
        "cancelled" | "canceled" | "void" => return Some("cancelled"),
        _ => return Some("unsupported_status"),
    }
    if !unexpired {
        return Some("expired");
    }
    if received_zatoshis > 0 || detected {
        return Some("payment_in_progress");
    }
    // The scanner can refuse direct transfers once a cross-chain quote exists.
    if crosschain_started {
        return Some("crosschain_in_progress");
    }
    None
}

/// Exact decimal ZEC (at most 8 fractional digits) to zatoshis, no floats.
pub fn decimal_zec_to_zatoshis(value: &str) -> Option<i64> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() || whole.len() > 8 || frac.len() > 8 {
        return None;
    }
    if !whole.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = whole.parse().ok()?;
    let frac: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<8}").parse().ok()?
    };
    whole.checked_mul(100_000_000)?.checked_add(frac)
}

fn memo_text(b64: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(b64.trim_end_matches('='))
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// Parse the ZIP-321 URIs this service writes: a single payment
/// (`zcash:ADDR?amount=&memo=`) or indexed payments (`zcash:?address=...&address.1=...`).
pub fn parse_uri(uri: &str) -> Option<Vec<(String, i64, String)>> {
    let rest = uri.strip_prefix("zcash:")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut slots: std::collections::BTreeMap<u32, [Option<String>; 3]> = Default::default();
    if !path.is_empty() {
        slots.entry(0).or_default()[0] = Some(path.to_string());
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=')?;
        let (name, index) = match key.split_once('.') {
            Some((name, idx)) => {
                if idx.is_empty() || idx.starts_with('0') || !idx.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                (name, idx.parse::<u32>().ok()?)
            }
            None => (key, 0),
        };
        let field = match name {
            "address" => 0,
            "amount" => 1,
            "memo" => 2,
            _ => return None,
        };
        let slot = &mut slots.entry(index).or_default()[field];
        if slot.is_some() {
            return None;
        }
        *slot = Some(value.to_string());
    }
    let mut out = Vec::new();
    for [address, amount, memo] in slots.into_values() {
        let amount = decimal_zec_to_zatoshis(&amount?)?;
        let memo = memo_text(&memo?)?;
        out.push((address?, amount, memo));
    }
    (!out.is_empty()).then_some(out)
}

/// Outputs of the invoice's payment request. The invoice output
/// uses the stored integer `price_zatoshis`; any extra output must be the
/// platform fee for this invoice.
pub fn outputs(
    uri: &str,
    invoice_id: &str,
    payment_address: &str,
    memo_code: &str,
    price_zatoshis: i64,
) -> Option<Vec<Output>> {
    let parsed = parse_uri(uri)?;
    let fee_memo = format!("FEE-{invoice_id}");
    let mut result = Vec::with_capacity(parsed.len());
    for (index, (address, amount, memo)) in parsed.into_iter().enumerate() {
        if index == 0 {
            // URI amounts are formatted from a float; allow 1 zatoshi of
            // formatting difference but never pay less than the stored price.
            if address != payment_address || memo != memo_code || (amount - price_zatoshis).abs() > 1 {
                return None;
            }
            result.push(Output { address, amount_zatoshis: price_zatoshis, memo, purpose: "invoice", required: true });
        } else if index == 1 && memo == fee_memo && amount > 0 {
            result.push(Output { address, amount_zatoshis: amount, memo, purpose: "platform_fee", required: false });
        } else {
            return None;
        }
    }
    (price_zatoshis > 0).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }

    #[test]
    fn decimal_amounts_are_exact() {
        assert_eq!(decimal_zec_to_zatoshis("0.00000001"), Some(1));
        assert_eq!(decimal_zec_to_zatoshis("1.5"), Some(150_000_000));
        assert_eq!(decimal_zec_to_zatoshis("12"), Some(1_200_000_000));
        assert_eq!(decimal_zec_to_zatoshis("0.123456789"), None);
        assert_eq!(decimal_zec_to_zatoshis("-1"), None);
        assert_eq!(decimal_zec_to_zatoshis(".5"), None);
        assert_eq!(decimal_zec_to_zatoshis("1e3"), None);
    }

    #[test]
    fn single_and_fee_uris_parse_to_outputs() {
        let single = format!("zcash:u1merchant?amount={:.8}&memo={}", 0.1234_f64, b64("CP-AB12"));
        assert_eq!(
            outputs(&single, "inv", "u1merchant", "CP-AB12", 12_340_000).unwrap(),
            vec![Output { address: "u1merchant".into(), amount_zatoshis: 12_340_000, memo: "CP-AB12".into(), purpose: "invoice", required: true }]
        );
        let fee = format!(
            "zcash:?address=u1merchant&amount={:.8}&memo={}&address.1=u1fee&amount.1={:.8}&memo.1={}",
            0.1234_f64, b64("CP-AB12"), 0.001234_f64, b64("FEE-inv")
        );
        let out = outputs(&fee, "inv", "u1merchant", "CP-AB12", 12_340_000).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], Output { address: "u1fee".into(), amount_zatoshis: 123_400, memo: "FEE-inv".into(), purpose: "platform_fee", required: false });
    }

    #[test]
    fn inconsistent_or_unknown_uris_are_refused() {
        let memo = b64("CP-AB12");
        // Wrong address, memo, amount, foreign fee memo, unknown parameter, duplicate key.
        for uri in [
            format!("zcash:u1other?amount=0.1&memo={memo}"),
            format!("zcash:u1merchant?amount=0.1&memo={}", b64("CP-OTHER")),
            format!("zcash:u1merchant?amount=0.2&memo={memo}"),
            format!("zcash:?address=u1merchant&amount=0.1&memo={memo}&address.1=u1fee&amount.1=0.001&memo.1={}", b64("FEE-other")),
            format!("zcash:u1merchant?amount=0.1&memo={memo}&label=x"),
            format!("zcash:u1merchant?amount=0.1&amount=0.1&memo={memo}"),
            "zcash:u1merchant?amount=0.1".to_string(),
        ] {
            assert!(outputs(&uri, "inv", "u1merchant", "CP-AB12", 10_000_000).is_none(), "{uri}");
        }
    }

    #[test]
    fn only_fresh_untouched_pending_invoices_are_payable() {
        assert_eq!(not_payable_reason("pending", true, 0, false, false), None);
        assert_eq!(not_payable_reason("pending", false, 0, false, false), Some("expired"));
        assert_eq!(not_payable_reason("pending", true, 5, false, false), Some("payment_in_progress"));
        assert_eq!(not_payable_reason("pending", true, 0, true, false), Some("payment_in_progress"));
        assert_eq!(not_payable_reason("pending", true, 0, false, true), Some("crosschain_in_progress"));
        assert_eq!(not_payable_reason("underpaid", true, 5, false, false), Some("underpaid"));
        assert_eq!(not_payable_reason("confirmed", true, 0, false, false), Some("already_paid"));
        assert_eq!(not_payable_reason("draft", true, 0, false, false), Some("draft"));
        assert_eq!(not_payable_reason("mystery", true, 0, false, false), Some("unsupported_status"));
    }
}
