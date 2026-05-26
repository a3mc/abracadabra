//! `solana_runtime::bank` — `bank frozen: ...` events.
//!
//! Format (sample, single line):
//! ```text
//! bank frozen: 1028070 hash: FeYBiF7s... signature_count: 23613 last_blockhash: HnhqoS8K... \
//!   capitalization: 502787631450148825, accounts_lt_hash checksum: H2hMuXb7..., \
//!   stats: BankHashStats { ... }
//! ```
//! v0.1 only extracts `(slot, hash, signature_count)`. Other fields can be
//! parsed lazily in later tasks.

use std::sync::OnceLock;

use regex::Regex;

use crate::parser::EventKind;

const HASH_CHARS: &str = "[1-9A-HJ-NP-Za-km-z]+";

pub fn parse_body(body: &str) -> Option<EventKind> {
    let caps = re_bank_frozen().captures(body)?;
    let slot = caps[1].parse().ok()?;
    let hash = caps[2].to_owned();
    let signature_count = caps[3].parse().ok()?;
    Some(EventKind::BankFrozen {
        slot,
        hash,
        signature_count,
    })
}

fn must_compile(pattern: &str) -> Regex {
    #[allow(clippy::expect_used)]
    Regex::new(pattern).expect("static regex must compile")
}

fn re_bank_frozen() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^bank frozen: ([0-9]+) hash: ({HASH_CHARS}) signature_count: ([0-9]+)"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_frozen_extracts_slot_hash_sig() {
        let line = "bank frozen: 1028070 hash: FeYBiF7syZ7SS5vAjrRQNmmqUAkD5TSEMbRN1KfbCdVB \
                    signature_count: 23613 last_blockhash: HnhqoS8KRrp6HbSLt332Ds1UsEGjhmnttDTsQAxm9CGd \
                    capitalization: 502787631450148825, accounts_lt_hash checksum: H2hMuXb7";
        let ev = parse_body(line).unwrap();
        let EventKind::BankFrozen {
            slot,
            hash,
            signature_count,
        } = ev
        else {
            panic!("expected BankFrozen");
        };
        assert_eq!(slot, 1_028_070);
        assert!(hash.starts_with("FeYBiF7"));
        assert_eq!(signature_count, 23_613);
    }

    #[test]
    fn unrelated_body_is_none() {
        assert!(parse_body("Bank dropped: 12345").is_none());
        assert!(parse_body("").is_none());
    }
}
