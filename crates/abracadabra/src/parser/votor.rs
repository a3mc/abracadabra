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

use crate::parser::EventKind;

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
        let (slot_str, hash) = after.split_once(' ')?;
        let slot = slot_str.parse().ok()?;
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
fn parse_tuple(s: &str) -> Option<(u64, String)> {
    let inner = s.strip_prefix('(')?;
    let close = inner.find(')')?;
    let pair = &inner[..close];
    let (slot_str, hash) = pair.split_once(", ")?;
    Some((slot_str.parse().ok()?, hash.to_owned()))
}

// ---- Static regex cache ----

fn must_compile(pattern: &str) -> Regex {
    // Static regex strings are compile-time programmer-validated; failure here
    // indicates a bug in this file, not a runtime input issue.
    #[allow(clippy::expect_used)]
    Regex::new(pattern).expect("static regex must compile")
}

// Base58 alphabet (Bitcoin/Solana): excludes 0, O, I, l.
const HASH_CHARS: &str = "[1-9A-HJ-NP-Za-km-z]+";

fn re_block_with_parent() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^\(([0-9]+), ({HASH_CHARS})\) parent \(([0-9]+), ({HASH_CHARS})\)$"
        ))
    })
}

fn re_finalized() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^Finalized \(([0-9]+), ({HASH_CHARS})\) fast: (true|false)$"
        ))
    })
}

fn re_produce_window() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^ProduceWindow LeaderWindowInfo \{{ start_slot: ([0-9]+), end_slot: ([0-9]+), parent_block: \(([0-9]+), ({HASH_CHARS})\)"
        ))
    })
}

fn re_standstill_ended() -> &'static Regex {
    // Input here has already had "Standstill initially detected at slot=" stripped.
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| must_compile(r"^([0-9]+) has ended at slot=([0-9]+)\."))
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
}
