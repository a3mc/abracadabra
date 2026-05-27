//! `agave_votor::event_handler` — the consensus event vocabulary.
//!
//! The caller has already stripped the `[ts LEVEL  module]` prefix; we receive
//! the message body, which starts with `<pubkey>: <event-text>`. We strip the
//! pubkey here and dispatch on the first token of the event text.
//!
//! Dispatch is keyed on the first word to avoid running every regex on every
//! line. Patterns with tuple payloads (Block, Finalized, SafeToNotar,
//! ProduceWindow, StandstillEnded) use regex; the rest use plain `strip_prefix`.

use std::sync::OnceLock;

use regex::Regex;

use crate::parser::{must_compile, validate_base58_hash, EventKind, HASH_CHARS, SLOT_DIGITS};

/// Parse the body of an `agave_votor::event_handler` info-log line.
///
/// The `body` parameter is the substring after `module] ` produced by
/// `line::parse_prefix`. Returns `None` for unrecognised event text.
pub fn parse_body(body: &str) -> Option<EventKind> {
    let (_pubkey, event) = body.split_once(": ")?;
    let head = first_word(event);
    match head {
        "Block" => parse_block_variant(event),
        "Voting" => parse_voting_variant(event),
        "First" => parse_first_shred(event),
        "Timeout" => parse_timeout(event),
        "TimeoutCrashedLeader" => parse_timeout_crashed_leader(event),
        "SafeToNotar" => parse_safe_to_notar(event),
        "SafeToSkip" => parse_safe_to_skip(event),
        "ProduceWindow" => parse_produce_window(event),
        "Standstill" => parse_standstill_variant(event),
        "Extending" => parse_standstill_extending(event),
        "Finalized" => parse_finalized(event),
        "SetIdentity" => Some(EventKind::SetIdentity),
        "Refreshing" => parse_refreshing(event),
        _ => None,
    }
}

fn first_word(s: &str) -> &str {
    s.split_once(' ').map_or(s, |(head, _)| head)
}

// ---- Block / Block Notarized / Block notar-fallback ----

fn parse_block_variant(event: &str) -> Option<EventKind> {
    let after = event.strip_prefix("Block ")?;
    if after.starts_with('(') {
        parse_block_with_parent(after)
    } else if let Some(rest) = after.strip_prefix("Notarized ") {
        let (slot, hash) = parse_tuple(rest)?;
        Some(EventKind::BlockNotarized { slot, hash })
    } else if let Some(rest) = after.strip_prefix("notar-fallback ") {
        let (slot, hash) = parse_tuple(rest)?;
        Some(EventKind::BlockNotarFallback { slot, hash })
    } else {
        None
    }
}

fn parse_block_with_parent(after_block: &str) -> Option<EventKind> {
    let re = re_block_with_parent();
    let caps = re.captures(after_block)?;
    let slot = caps[1].parse().ok()?;
    let hash = caps[2].to_owned();
    let parent_slot = caps[3].parse().ok()?;
    let parent_hash = caps[4].to_owned();
    Some(EventKind::Block {
        slot,
        hash,
        parent_slot,
        parent_hash,
    })
}

// ---- Voting notarize / finalize / skip ----

fn parse_voting_variant(event: &str) -> Option<EventKind> {
    let rest = event.strip_prefix("Voting ")?;
    if let Some(after) = rest.strip_prefix("notarize for ") {
        let (slot_str, after_slot) = after.split_once(' ')?;
        let slot = slot_str.parse().ok()?;
        // Take the leading Base58 run as the hash; require everything after
        // to be whitespace-only so trailing fields (e.g. " (forced)") cannot
        // be silently swallowed into the hash string.
        let hash_end = after_slot
            .bytes()
            .position(|b| !super::is_base58_byte(b))
            .unwrap_or(after_slot.len());
        let hash = validate_base58_hash(&after_slot[..hash_end])?;
        // ASCII-only tail check; the hash itself is ASCII-bounded so
        // Unicode whitespace at the tail would be inconsistent.
        if !after_slot.as_bytes()[hash_end..]
            .iter()
            .all(|b| matches!(b, b' ' | b'\t'))
        {
            return None;
        }
        Some(EventKind::VotingNotarize {
            slot,
            hash: hash.to_owned(),
        })
    } else if let Some(after) = rest.strip_prefix("finalize for ") {
        Some(EventKind::VotingFinalize {
            slot: after.parse().ok()?,
        })
    } else if let Some(after) = rest.strip_prefix("skip for ") {
        Some(EventKind::VotingSkip {
            slot: after.parse().ok()?,
        })
    } else {
        None
    }
}

// ---- Single-slot events ----

fn parse_first_shred(event: &str) -> Option<EventKind> {
    let slot = event.strip_prefix("First shred ")?.parse().ok()?;
    Some(EventKind::FirstShred { slot })
}

fn parse_timeout(event: &str) -> Option<EventKind> {
    let slot = event.strip_prefix("Timeout ")?.parse().ok()?;
    Some(EventKind::Timeout { slot })
}

fn parse_timeout_crashed_leader(event: &str) -> Option<EventKind> {
    let slot = event.strip_prefix("TimeoutCrashedLeader ")?.parse().ok()?;
    Some(EventKind::TimeoutCrashedLeader { slot })
}

fn parse_safe_to_skip(event: &str) -> Option<EventKind> {
    let slot = event.strip_prefix("SafeToSkip ")?.parse().ok()?;
    Some(EventKind::SafeToSkip { slot })
}

// ---- Tuple-payload events ----

fn parse_safe_to_notar(event: &str) -> Option<EventKind> {
    let rest = event.strip_prefix("SafeToNotar ")?;
    let (slot, hash) = parse_tuple(rest)?;
    Some(EventKind::SafeToNotar { slot, hash })
}

fn parse_finalized(event: &str) -> Option<EventKind> {
    let re = re_finalized();
    let caps = re.captures(event)?;
    let slot = caps[1].parse().ok()?;
    let hash = caps[2].to_owned();
    let fast = match &caps[3] {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    Some(EventKind::Finalized { slot, hash, fast })
}

fn parse_produce_window(event: &str) -> Option<EventKind> {
    let re = re_produce_window();
    let caps = re.captures(event)?;
    let start = caps[1].parse().ok()?;
    let end = caps[2].parse().ok()?;
    let parent_slot = caps[3].parse().ok()?;
    let parent_hash = caps[4].to_owned();
    Some(EventKind::ProduceWindow {
        start,
        end,
        parent_slot,
        parent_hash,
    })
}

// ---- Standstill variants ----

fn parse_standstill_variant(event: &str) -> Option<EventKind> {
    if let Some(rest) = event.strip_prefix("Standstill initially detected at slot=") {
        // "Standstill initially detected at slot=X has ended at slot=Y. Ending timeout extension"
        let re = re_standstill_ended();
        let caps = re.captures(rest)?;
        let entry_slot = caps[1].parse().ok()?;
        let exit_slot = caps[2].parse().ok()?;
        Some(EventKind::StandstillEnded {
            entry_slot,
            exit_slot,
        })
    } else if let Some(rest) = event.strip_prefix("Standstill ") {
        // "Standstill SLOT"
        Some(EventKind::Standstill {
            slot: rest.parse().ok()?,
        })
    } else {
        None
    }
}

fn parse_standstill_extending(event: &str) -> Option<EventKind> {
    let rest = event.strip_prefix("Extending timeouts starting at slot ")?;
    Some(EventKind::StandstillExtending {
        slot: rest.parse().ok()?,
    })
}

// ---- Refreshing vote ----

fn parse_refreshing(event: &str) -> Option<EventKind> {
    // "Refreshing vote {vote:?}" — body details parsed in a later task.
    if event.starts_with("Refreshing vote ") {
        Some(EventKind::RefreshingVote)
    } else {
        None
    }
}

// ---- Shared helpers ----

/// Parse `(SLOT, HASH)` into `(u64, String)`.
///
/// Used by the `strip_prefix` dispatch paths (`Block Notarized`, `Block
/// notar-fallback`, `SafeToNotar`) which — unlike the regex paths — have
/// no inline alphabet check. The hash is length- and alphabet-validated
/// via `validate_base58_hash` to match the regex paths' `HASH_CHARS`
/// bound; the trailing slice after `)` must be ASCII whitespace only,
/// mirroring `VotingNotarize` (PARSE-02) so a future emitter that
/// appends a trailing field cannot be silently swallowed.
fn parse_tuple(s: &str) -> Option<(u64, String)> {
    let inner = s.strip_prefix('(')?;
    let close = inner.find(')')?;
    let pair = &inner[..close];
    let tail = &inner[close + 1..];
    if !tail.as_bytes().iter().all(|b| matches!(b, b' ' | b'\t')) {
        return None;
    }
    let (slot_str, hash) = pair.split_once(", ")?;
    let slot = slot_str.parse().ok()?;
    let hash = validate_base58_hash(hash)?.to_owned();
    Some((slot, hash))
}

// ---- Static regex cache ----

// `HASH_CHARS` and `SLOT_DIGITS` live in `parser/mod.rs` so the
// `strip_prefix` paths and the regex paths share one length policy.

fn re_block_with_parent() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^\(({SLOT_DIGITS}), ({HASH_CHARS})\) parent \(({SLOT_DIGITS}), ({HASH_CHARS})\)$"
        ))
    })
}

fn re_finalized() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^Finalized \(({SLOT_DIGITS}), ({HASH_CHARS})\) fast: (true|false)$"
        ))
    })
}

fn re_produce_window() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^ProduceWindow LeaderWindowInfo \{{ start_slot: ({SLOT_DIGITS}), end_slot: ({SLOT_DIGITS}), parent_block: \(({SLOT_DIGITS}), ({HASH_CHARS})\)"
        ))
    })
}

fn re_standstill_ended() -> &'static Regex {
    // Input here has already had "Standstill initially detected at slot=" stripped.
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^({SLOT_DIGITS}) has ended at slot=({SLOT_DIGITS})\."
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim samples lifted from docs/alpenglow/log-strings-reference.md.
    // Each test feeds the parser the same body shape it would see at runtime:
    // `<pubkey>: <event-text>`.

    const PK: &str = "ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu";

    fn body(s: &str) -> String {
        format!("{PK}: {s}")
    }

    #[test]
    fn block_with_parent() {
        let s = body(
            "Block (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv) \
             parent (1028069, CdJR4iF3xpkfSH62aMfBfJqKdpTR55KvFnHN93kPDUaW)",
        );
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::Block {
                slot: 1028070,
                parent_slot: 1028069,
                ..
            }
        ));
    }

    #[test]
    fn block_notarized() {
        let s = body("Block Notarized (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv)");
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::BlockNotarized { slot: 1028070, .. }
        ));
    }

    #[test]
    fn block_notar_fallback() {
        let s =
            body("Block notar-fallback (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv)");
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::BlockNotarFallback { slot: 1028070, .. }
        ));
    }

    #[test]
    fn voting_notarize() {
        let s = body("Voting notarize for 1028070 EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv");
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::VotingNotarize { slot: 1028070, .. }
        ));
    }

    #[test]
    fn voting_finalize() {
        let s = body("Voting finalize for 1028070");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::VotingFinalize { slot: 1028070 }
        ));
    }

    #[test]
    fn voting_skip() {
        let s = body("Voting skip for 1028084");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::VotingSkip { slot: 1028084 }
        ));
    }

    #[test]
    fn first_shred() {
        let s = body("First shred 1028071");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::FirstShred { slot: 1028071 }
        ));
    }

    #[test]
    fn timeout() {
        let s = body("Timeout 1028084");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::Timeout { slot: 1028084 }
        ));
    }

    #[test]
    fn timeout_crashed_leader() {
        let s = body("TimeoutCrashedLeader 1028084");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::TimeoutCrashedLeader { slot: 1028084 }
        ));
    }

    #[test]
    fn safe_to_notar() {
        let s = body("SafeToNotar (1051172, DTBC1p4b31RH7hRZFZxg4pSxwrsyE4ycmZrTKcTc6ygz)");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::SafeToNotar { slot: 1051172, .. }
        ));
    }

    #[test]
    fn safe_to_skip() {
        let s = body("SafeToSkip 1113669");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::SafeToSkip { slot: 1113669 }
        ));
    }

    #[test]
    fn produce_window() {
        let s = body(
            "ProduceWindow LeaderWindowInfo { \
             start_slot: 1028248, end_slot: 1028251, \
             parent_block: (1028247, GG5ybXkSgf97V5BWgRFQKkweMMvabhaMy16XPsNtjwbB), \
             block_timer: Instant { tv_sec: 654042, tv_nsec: 317064752 } }",
        );
        let ev = parse_body(&s).unwrap();
        let EventKind::ProduceWindow {
            start,
            end,
            parent_slot,
            ..
        } = ev
        else {
            panic!("expected ProduceWindow");
        };
        assert_eq!(start, 1_028_248);
        assert_eq!(end, 1_028_251);
        assert_eq!(parent_slot, 1_028_247);
    }

    #[test]
    fn finalized_fast_true() {
        let s =
            body("Finalized (1207084, 5RBfaAmnWYr2R4WVRHUatyMdpKQr7FU9yeqHuwzBgpqc) fast: true");
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::Finalized {
                slot: 1207084,
                fast: true,
                ..
            }
        ));
    }

    #[test]
    fn finalized_fast_false() {
        let s =
            body("Finalized (1207084, 5RBfaAmnWYr2R4WVRHUatyMdpKQr7FU9yeqHuwzBgpqc) fast: false");
        let ev = parse_body(&s).unwrap();
        assert!(matches!(ev, EventKind::Finalized { fast: false, .. }));
    }

    #[test]
    fn standstill_simple() {
        let s = body("Standstill 1234567");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::Standstill { slot: 1234567 }
        ));
    }

    #[test]
    fn standstill_extending() {
        let s = body("Extending timeouts starting at slot 1234567");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::StandstillExtending { slot: 1234567 }
        ));
    }

    #[test]
    fn standstill_ended() {
        let s = body(
            "Standstill initially detected at slot=1234567 has ended at \
             slot=1234800. Ending timeout extension",
        );
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::StandstillEnded {
                entry_slot: 1234567,
                exit_slot: 1234800,
            }
        ));
    }

    #[test]
    fn set_identity() {
        let s = body("SetIdentity");
        assert!(matches!(parse_body(&s).unwrap(), EventKind::SetIdentity));
    }

    #[test]
    fn refreshing_vote() {
        let s = body("Refreshing vote Notarize(NotarizationVote { slot: 1234, block_id: Foo })");
        assert!(matches!(parse_body(&s).unwrap(), EventKind::RefreshingVote));
    }

    #[test]
    fn unknown_body_returns_none() {
        let s = body("SomeFutureEventWeDontHandle 1234");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn malformed_body_returns_none() {
        // Missing pubkey prefix.
        assert!(parse_body("Voting notarize for 1028070 EEZ").is_none());
        // Truncated tuple.
        assert!(parse_body(&body("Block Notarized (123, ")).is_none());
    }

    // ---- PARSE-02: Voting notarize must not silently swallow trailing fields ----

    #[test]
    fn voting_notarize_trailing_junk_rejected() {
        // If agave appends a trailing field (`(forced)`), we must NOT capture
        // it into the hash string. Adopt strict rejection rather than partial
        // accept so the regression surfaces immediately.
        let s = body(
            "Voting notarize for 1028070 EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn voting_notarize_trailing_whitespace_accepted() {
        // Pure trailing whitespace is benign.
        let s = body("Voting notarize for 1028070 EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv  ");
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::VotingNotarize { slot: 1028070, .. }
        ));
    }

    #[test]
    fn voting_notarize_non_base58_hash_rejected() {
        // Hash containing `0`/`O`/`I`/`l` is not valid Base58.
        let s = body("Voting notarize for 1028070 OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        assert!(parse_body(&s).is_none());
    }

    // ---- PARSE-01: tuple-payload hashes must be Base58-validated ----

    #[test]
    fn block_notarized_garbage_hash_rejected() {
        // OIl0 contains four invalid Base58 chars; previously accepted.
        let s = body("Block Notarized (123, OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA)");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn safe_to_notar_garbage_hash_rejected() {
        let s = body("SafeToNotar (1051172, OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA)");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notar_fallback_garbage_hash_rejected() {
        let s =
            body("Block notar-fallback (1028070, OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA)");
        assert!(parse_body(&s).is_none());
    }

    // ---- PARSE-04: numeric-overflow handling ----

    #[test]
    fn slot_overflow_returns_none() {
        // 2^64 = 18_446_744_073_709_551_616 — one past u64::MAX.
        let s = body("First shred 18446744073709551616");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn slot_overflow_in_tuple_returns_none() {
        let s = body(
            "Block Notarized (18446744073709551616, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv)",
        );
        assert!(parse_body(&s).is_none());
    }

    // ---- PARSE-05: tuple-payload hash length must be bounded (32..=48) ----

    #[test]
    fn block_notarized_short_hash_rejected() {
        // 4-char hash is base58-alphabet-valid but below the 32-char minimum.
        let s = body("Block Notarized (1028070, EEZ7)");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notarized_overlong_hash_rejected() {
        // 49 chars exceeds the 48-char maximum.
        let h = "A".repeat(49);
        let s = body(&format!("Block Notarized (1028070, {h})"));
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notar_fallback_short_hash_rejected() {
        let s = body("Block notar-fallback (1028070, EEZ7)");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn safe_to_notar_short_hash_rejected() {
        let s = body("SafeToNotar (1028070, EEZ7)");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn voting_notarize_short_hash_rejected() {
        let s = body("Voting notarize for 1028070 abcd");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn voting_notarize_overlong_hash_rejected() {
        let h = "A".repeat(49);
        let s = body(&format!("Voting notarize for 1028070 {h}"));
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notarized_hash_at_min_length_accepted() {
        // 32 chars is exactly the lower bound; must accept.
        let h = "1".repeat(32);
        let s = body(&format!("Block Notarized (1028070, {h})"));
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::BlockNotarized { slot: 1028070, .. }
        ));
    }

    #[test]
    fn block_notarized_hash_at_max_length_accepted() {
        // 48 chars is exactly the upper bound; must accept.
        let h = "1".repeat(48);
        let s = body(&format!("Block Notarized (1028070, {h})"));
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::BlockNotarized { slot: 1028070, .. }
        ));
    }

    // ---- PARSE-06: tuple-payload events must reject trailing junk ----

    #[test]
    fn block_notarized_trailing_junk_rejected() {
        let s = body(
            "Block Notarized (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv) (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notar_fallback_trailing_junk_rejected() {
        let s = body(
            "Block notar-fallback (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv) (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn safe_to_notar_trailing_junk_rejected() {
        let s =
            body("SafeToNotar (1051172, DTBC1p4b31RH7hRZFZxg4pSxwrsyE4ycmZrTKcTc6ygz) (forced)");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notarized_trailing_whitespace_accepted() {
        // Pure trailing whitespace is benign, matching PARSE-02's stance on VotingNotarize.
        let s = body("Block Notarized (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv)  ");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::BlockNotarized { slot: 1028070, .. }
        ));
    }

    // ---- PARSE-10: VotingNotarize tail must be ASCII whitespace only ----

    #[test]
    fn voting_notarize_nbsp_tail_rejected() {
        // U+00A0 (NBSP) is Unicode whitespace but not ASCII; must reject.
        let s = body(
            "Voting notarize for 1028070 EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv\u{00A0}",
        );
        assert!(parse_body(&s).is_none());
    }
}
