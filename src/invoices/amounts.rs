pub(crate) const MIN_FEE_ZEC: f64 = 0.0001;

pub fn zatoshis_to_zec(z: i64) -> f64 {
    format!("{:.8}", z as f64 / 100_000_000.0)
        .parse::<f64>()
        .unwrap_or(0.0)
}

pub fn zec_to_zatoshis(amount_zec: f64) -> anyhow::Result<i64> {
    if !amount_zec.is_finite() || amount_zec < 0.0 {
        anyhow::bail!("Invalid ZEC amount");
    }

    let scaled = (amount_zec * 100_000_000.0).round();
    if scaled < 0.0 || scaled > i64::MAX as f64 {
        anyhow::bail!("ZEC amount out of range");
    }

    Ok(scaled as i64)
}

#[cfg(test)]
mod tests {
    use super::zec_to_zatoshis;

    #[test]
    fn rounds_to_nearest_zatoshi() {
        assert_eq!(zec_to_zatoshis(0.000000016).unwrap(), 2);
        assert_eq!(zec_to_zatoshis(1.234567895).unwrap(), 123_456_790);
    }
}
