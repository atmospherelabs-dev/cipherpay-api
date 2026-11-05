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
        use orchard::keys::{FullViewingKey, SpendingKey};
        use zcash_address::unified::{Fvk, Ufvk};
        let sk = SpendingKey::from_bytes([7u8; 32]).unwrap();
        let fvk = FullViewingKey::from(&sk);
        let test_key = Ufvk::try_from_items(vec![Fvk::Orchard(fvk.to_bytes())])
            .unwrap()
            .encode(&zcash_protocol::consensus::NetworkType::Main);

        let addr0 = derive_invoice_address(&test_key, 0).unwrap();
        let addr1 = derive_invoice_address(&test_key, 1).unwrap();

        assert_ne!(addr0.ua_string, addr1.ua_string);
        assert_ne!(addr0.orchard_receiver_hex, addr1.orchard_receiver_hex);
    }

    /// Golden values produced by the pre-NU7 production build (b902f50, Zakura 1.x
    /// crates). Existing invoices are matched by these receivers, so a Zcash crate
    /// upgrade must derive byte-identical keys and addresses.
    #[test]
    fn derivation_matches_pre_nu7_production_build() {
        use orchard::keys::{FullViewingKey, SpendingKey};
        use zcash_address::unified::{Fvk, Ufvk};
        use zcash_protocol::consensus::NetworkType;

        const RECEIVERS: [(u32, &str); 4] = [
            (0, "4559029c0b5dbf941c5ad181a5fe8f45b34630f29d0c8dd8dc1cc3573386f416cb324133156d723df5e62d"),
            (1, "37f1c1f4c922ecb98f3cf2dbe6f12f65a73fc93316a1c0ad4a23a8a3b9b4eadbb7687b6e82161d031698bf"),
            (7, "75bef073ad7a0fb0ab7d8aff0284d9f57d9efb6b32ab6916cbbb119961d55f45a3c719edee4a2e5acbff84"),
            (1000, "6a61da5631788cc6ecc9db8e8cbaaf8d3f3b5ea81a2704c9da47a060274f257d5a044fb39ae3ebe14977af"),
        ];
        let cases = [
            (
                NetworkType::Main,
                "uivk1ewu2n4n4xgwxvem5krz36flunule40kyss82myn583hj9ypsrmt34yw3t5h6utkcx45w3tcharxck5fzurycymxp408wxh7f7u2snqdekm8d9au580fvm7yj9y3qsw7d4l9qsmsz98",
                [
                    "u1u4zdvtcrjt4cnffw3shrx440754g0mhr4a7fenck686tfr6rdqy9wu5v39ydpgm4ut37qnlh9kpw9fsp8wcwyu2y2r5stjhj0qwfnmrq",
                    "u16p5xgserze3c9lj4pyuxhesqsssllaunzhehy2y0se5r8ac0tn7eaz4525kvf84sxhahngkd3wp3vlcydapyqyhehttncc0u0y4kqqje",
                    "u12lqydgardgp2g3d8ndvl09zycau3a008njs3uww4a2j2l9yvnul5g5lvqw6f6v08h00jy46p84qq2uaqmrwfp3n6yx7wzadq8vd0dc4h",
                    "u1rg3yrz2c34qgky548d57s8nvts8txja4m04ug4l5v7nfaxwhjrpjtynjtkqjdyhpwmqy4nn8624wftr84dgdnmm5nalzcpqe4g3cfsk7",
                ],
            ),
            (
                NetworkType::Test,
                "uivktest1zk0r9g2yavyhv03qdeexrwkfzp5vt7sspfa3fstt7pz3z0zwgw6gppm96vp5fd8atsftgn92agytakhwshmswsrxrgzprzzyzq63dyvvpagd3ktt8qvwpu3xpcmgjk0x9a0qaqjz24",
                [
                    "utest1j6uf5arygvvnypvh7n9pgcmt70xapzx70t3nemk2yy6646ekn2d296wyc355pvjtwnftcaukuqv687wnecvs47h9w82z4v498c58d6w8",
                    "utest1scuz7xf9wxep7xzuf5utvwclg649svw8wk2z90zdygpza0mkpe4070nleu5me07j8g7re27ed2y2qy5p5vtwgcf0vrr3ym33jve4eafm",
                    "utest1m7pvrhnnpjajfww846shv52puz5palj068m83e4mdklw70ckmt7nmfluap4vuguh7hm9pr53k4sne8dxnvrvlkqn646a822ppgfq074s",
                    "utest1p5wvwuhera8t7pp2z3uhelvpxfkv2e5e2302kjt45lwyf3lw6qtyr9ul2954tkcrg9qp3d593d6zs5q4g8lxhpkpvldruwrqpyw4vtvg",
                ],
            ),
        ];

        let fvk = FullViewingKey::from(&SpendingKey::from_bytes([7u8; 32]).unwrap());
        for (network, expected_uivk, uas) in cases {
            let ufvk = Ufvk::try_from_items(vec![Fvk::Orchard(fvk.to_bytes())])
                .unwrap()
                .encode(&network);
            let uivk = crate::scanner::decrypt::derive_uivk_from_ufvk(&ufvk).unwrap();
            assert_eq!(uivk, expected_uivk, "{network:?} UIVK");
            for ((index, receiver), ua) in RECEIVERS.iter().zip(uas) {
                for key in [&ufvk, &uivk] {
                    let addr = derive_invoice_address(key, *index).unwrap();
                    assert_eq!(addr.orchard_receiver_hex, *receiver, "{network:?} #{index}");
                    assert_eq!(addr.ua_string, ua, "{network:?} #{index}");
                }
            }
        }
    }
}
