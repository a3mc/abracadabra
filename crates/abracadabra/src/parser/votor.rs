//! `agave_votor::event_handler` — the consensus event vocabulary.
//!
//! The caller has already stripped the `[ts LEVEL  module]` prefix; we receive
//! the message body, which starts with `<pubkey>: <event-text>`. We strip the
//! pubkey here and dispatch on the first token of the event text.
//!
//! Dispatch is keyed on the first word to avoid running every regex on every
//! line. Patterns with `Block`-struct payloads (Block, Finalized, SafeToNotar,
//! ProduceWindow, ParentReady) use regex; the rest use plain `strip_prefix`.

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
        "Triggering" => parse_triggering_parent_ready(event),
        "Parent" => parse_parent_ready(event),
        _ => None,
    }
}

fn first_word(s: &str) -> &str {
    s.split_once(' ').map_or(s, |(head, _)| head)
}

// ---- Block / Block Notarized / Block notar-fallback ----

fn parse_block_variant(event: &str) -> Option<EventKind> {
    let after = event.strip_prefix("Block ")?;
    if let Some(rest) = after.strip_prefix("Notarized ") {
        let (slot, hash) = parse_tuple(rest)?;
        Some(EventKind::BlockNotarized { slot, hash })
    } else if let Some(rest) = after.strip_prefix("notar-fallback ") {
        let (slot, hash) = parse_tuple(rest)?;
        Some(EventKind::BlockNotarFallback { slot, hash })
    } else if after.starts_with("Block { slot: ") {
        parse_block_with_parent(after)
    } else {
        None
    }
}

fn parse_block_with_parent(after_block: &str) -> Option<EventKind> {
    let caps = re_block_with_parent().captures(after_block)?;
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
    } else if let Some(after) = rest.strip_prefix("skip-fallback for ") {
        Some(EventKind::VotingSkipFallback {
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

// ---- Block-struct-payload events ----

fn parse_safe_to_notar(event: &str) -> Option<EventKind> {
    let rest = event.strip_prefix("SafeToNotar ")?;
    let (slot, hash) = parse_tuple(rest)?;
    Some(EventKind::SafeToNotar { slot, hash })
}

fn parse_finalized(event: &str) -> Option<EventKind> {
    let caps = re_finalized().captures(event)?;
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
    let caps = re_produce_window().captures(event)?;
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

// ---- Triggering parent ready ----

fn parse_triggering_parent_ready(event: &str) -> Option<EventKind> {
    let caps = re_triggering_parent_ready().captures(event)?;
    let slot = caps.get(1)?.as_str().parse().ok()?;
    let parent_slot = caps.get(2)?.as_str().parse().ok()?;
    let parent_hash = caps.get(3)?.as_str().to_owned();
    Some(EventKind::TriggeringParentReady {
        slot,
        parent_slot,
        parent_hash,
    })
}

// ---- Parent ready (normal-path ParentReadyTracker emit) ----

fn parse_parent_ready(event: &str) -> Option<EventKind> {
    let caps = re_parent_ready().captures(event)?;
    let slot = caps.get(1)?.as_str().parse().ok()?;
    let parent_slot = caps.get(2)?.as_str().parse().ok()?;
    let parent_hash = caps.get(3)?.as_str().to_owned();
    Some(EventKind::ParentReady {
        slot,
        parent_slot,
        parent_hash,
    })
}

// ---- Shared helpers ----

/// Parse `Block { slot: SLOT, block_id: HASH }REST` into `(u64, String)`.
///
/// `REST` must be ASCII whitespace only. Used by the `strip_prefix` dispatch
/// paths (`Block Notarized`, `Block notar-fallback`, `SafeToNotar`) which —
/// unlike the regex paths — have no inline alphabet check. The hash is length-
/// and alphabet-validated via `validate_base58_hash` to match the regex paths'
/// `HASH_CHARS` bound; the trailing slice after `}` must be ASCII whitespace
/// only, mirroring `VotingNotarize` (PARSE-02) so a future emitter that
/// appends a trailing field cannot be silently swallowed.
///
/// The tuple form `(SLOT, HASH)` was the pre-refactor Alpenglow Debug output
/// (`type Block = (Slot, Hash)`). Upstream refactored to
/// `struct Block { slot, block_id }` and the tuple form no longer emits;
/// the parser targets the struct form only.
fn parse_tuple(s: &str) -> Option<(u64, String)> {
    let inner = s.strip_prefix("Block { slot: ")?;
    let (slot_str, after_slot) = inner.split_once(", block_id: ")?;
    let slot: u64 = slot_str.parse().ok()?;
    let close = after_slot.find(" }")?;
    let hash = validate_base58_hash(&after_slot[..close])?.to_owned();
    let tail = &after_slot[close + 2..];
    if !tail.as_bytes().iter().all(|b| matches!(b, b' ' | b'\t')) {
        return None;
    }
    Some((slot, hash))
}

// ---- Static regex cache ----

// `HASH_CHARS` and `SLOT_DIGITS` live in `parser/mod.rs` so the
// `strip_prefix` paths and the regex paths share one length policy.

fn re_block_with_parent() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^Block \{{ slot: ({SLOT_DIGITS}), block_id: ({HASH_CHARS}) \}} parent Block \{{ slot: ({SLOT_DIGITS}), block_id: ({HASH_CHARS}) \}}$"
        ))
    })
}

fn re_finalized() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^Finalized Block \{{ slot: ({SLOT_DIGITS}), block_id: ({HASH_CHARS}) \}} fast: (true|false)$"
        ))
    })
}

fn re_produce_window() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^ProduceWindow LeaderWindowInfo \{{ start_slot: ({SLOT_DIGITS}), end_slot: ({SLOT_DIGITS}), parent_block: Block \{{ slot: ({SLOT_DIGITS}), block_id: ({HASH_CHARS}) \}}"
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

fn re_triggering_parent_ready() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^Triggering parent ready for slot ({SLOT_DIGITS}) with parent ({SLOT_DIGITS}) ({HASH_CHARS})$"
        ))
    })
}

fn re_parent_ready() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^Parent ready ({SLOT_DIGITS}) Block \{{ slot: ({SLOT_DIGITS}), block_id: ({HASH_CHARS}) \}}$"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim samples lifted from cadabra.log (2026-07-09 Alpenglow
    // 200ms-blocks capture). Each test feeds the parser the same body shape
    // it would see at runtime: `<pubkey>: <event-text>`.

    const PK: &str = "ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu";

    fn body(s: &str) -> String {
        format!("{PK}: {s}")
    }

    #[test]
    fn block_with_parent() {
        let s = body(
            "Block Block { slot: 702540, block_id: \
             6mARivgNupeinAsK1sssaP4P8QezwYwW73wb35Njr8jv } parent Block { slot: 702535, \
             block_id: 6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 }",
        );
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::Block {
                slot: 702540,
                parent_slot: 702535,
                ..
            }
        ));
    }

    #[test]
    fn block_notarized() {
        let s = body(
            "Block Notarized Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 }",
        );
        let ev = parse_body(&s).unwrap();
        assert!(matches!(ev, EventKind::BlockNotarized { slot: 702535, .. }));
    }

    #[test]
    fn block_notar_fallback() {
        let s = body(
            "Block notar-fallback Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 }",
        );
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::BlockNotarFallback { slot: 702535, .. }
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
    fn voting_skip_fallback() {
        let s = body("Voting skip-fallback for 282580");
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::VotingSkipFallback { slot: 282580 }
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
        let s = body(
            "SafeToNotar Block { slot: 706643, block_id: \
             EzNUMTPeenPo6d194TAzn3eonj6KJZWd7ynaachXjgqG }",
        );
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::SafeToNotar { slot: 706643, .. }
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
            "ProduceWindow LeaderWindowInfo { start_slot: 702540, \
             end_slot: 702543, parent_block: Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 }, block_timer: \
             Instant { tv_sec: 216365, tv_nsec: 811320941 } }",
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
        assert_eq!(start, 702_540);
        assert_eq!(end, 702_543);
        assert_eq!(parent_slot, 702_535);
    }

    #[test]
    fn finalized_fast_true() {
        let s = body(
            "Finalized Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 } fast: true",
        );
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::Finalized {
                slot: 702535,
                fast: true,
                ..
            }
        ));
    }

    #[test]
    fn finalized_fast_false() {
        let s = body(
            "Finalized Block { slot: 702664, block_id: \
             9sFV88boAJkE231ME6aj2uYcEYa6tp6BxFGVmx4SzrWJ } fast: false",
        );
        let ev = parse_body(&s).unwrap();
        assert!(matches!(
            ev,
            EventKind::Finalized {
                slot: 702664,
                fast: false,
                ..
            }
        ));
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
    fn triggering_parent_ready() {
        let s = body(
            "Triggering parent ready for slot 1028070 with parent 1028069 \
             CdJR4iF3xpkfSH62aMfBfJqKdpTR55KvFnHN93kPDUaW",
        );
        match parse_body(&s).unwrap() {
            EventKind::TriggeringParentReady {
                slot,
                parent_slot,
                parent_hash,
            } => {
                assert_eq!(slot, 1_028_070);
                assert_eq!(parent_slot, 1_028_069);
                assert_eq!(parent_hash, "CdJR4iF3xpkfSH62aMfBfJqKdpTR55KvFnHN93kPDUaW");
            }
            other => panic!("expected TriggeringParentReady, got {other:?}"),
        }
    }

    #[test]
    fn parent_ready_normal_path() {
        let s = body(
            "Parent ready 702536 Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 }",
        );
        match parse_body(&s).unwrap() {
            EventKind::ParentReady {
                slot,
                parent_slot,
                parent_hash,
            } => {
                assert_eq!(slot, 702_536);
                assert_eq!(parent_slot, 702_535);
                assert_eq!(parent_hash, "6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2");
            }
            other => panic!("expected ParentReady, got {other:?}"),
        }
    }

    #[test]
    fn parent_ready_garbage_hash_rejected() {
        // OIl0 contains four invalid Base58 chars.
        let s = body(
            "Parent ready 702536 Block { slot: 702535, block_id: \
             OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA }",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn parent_ready_distinct_from_triggering() {
        let normal = body(
            "Parent ready 702536 Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 }",
        );
        let trig = body(
            "Triggering parent ready for slot 702536 with parent 702535 \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2",
        );
        assert!(matches!(
            parse_body(&normal).unwrap(),
            EventKind::ParentReady { .. }
        ));
        assert!(matches!(
            parse_body(&trig).unwrap(),
            EventKind::TriggeringParentReady { .. }
        ));
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
        // Truncated struct payload.
        assert!(parse_body(&body("Block Notarized Block { slot: 123, block_id: ")).is_none());
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

    // ---- PARSE-01 + COV-02: struct-payload hashes must be Base58-validated ----

    #[test]
    fn block_notarized_garbage_hash_rejected() {
        // OIl0 contains four invalid Base58 chars.
        let s = body(
            "Block Notarized Block { slot: 123, block_id: \
             OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA }",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn safe_to_notar_garbage_hash_rejected() {
        let s = body(
            "SafeToNotar Block { slot: 1051172, block_id: \
             OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA }",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notar_fallback_garbage_hash_rejected() {
        let s = body(
            "Block notar-fallback Block { slot: 1028070, block_id: \
             OIl0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA }",
        );
        assert!(parse_body(&s).is_none());
    }

    // ---- PARSE-04 + COV-03: numeric-overflow handling ----

    #[test]
    fn slot_overflow_returns_none() {
        // 2^64 = 18_446_744_073_709_551_616 — one past u64::MAX.
        let s = body("First shred 18446744073709551616");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn slot_overflow_in_struct_returns_none() {
        // COV-03: struct-form slot overflow must return None.
        let s = body(
            "Block Notarized Block { slot: 18446744073709551616, block_id: \
             EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv }",
        );
        assert!(parse_body(&s).is_none());
    }

    // ---- PARSE-05 + COV-02: hash length must be bounded (32..=48) ----

    #[test]
    fn block_notarized_31_char_hash_rejected() {
        // COV-02: 31 chars is one below the 32-char minimum.
        let h = "1".repeat(31);
        let s = body(&format!(
            "Block Notarized Block {{ slot: 1028070, block_id: {h} }}"
        ));
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notarized_hash_at_min_length_accepted() {
        // COV-02: 32 chars is exactly the lower bound.
        let h = "1".repeat(32);
        let s = body(&format!(
            "Block Notarized Block {{ slot: 1028070, block_id: {h} }}"
        ));
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::BlockNotarized { slot: 1028070, .. }
        ));
    }

    #[test]
    fn block_notarized_hash_at_max_length_accepted() {
        // COV-02: 48 chars is exactly the upper bound.
        let h = "1".repeat(48);
        let s = body(&format!(
            "Block Notarized Block {{ slot: 1028070, block_id: {h} }}"
        ));
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::BlockNotarized { slot: 1028070, .. }
        ));
    }

    #[test]
    fn block_notarized_49_char_hash_rejected() {
        // COV-02: 49 chars exceeds the 48-char maximum.
        let h = "1".repeat(49);
        let s = body(&format!(
            "Block Notarized Block {{ slot: 1028070, block_id: {h} }}"
        ));
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notar_fallback_short_hash_rejected() {
        let s = body("Block notar-fallback Block { slot: 1028070, block_id: EEZ7 }");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn safe_to_notar_short_hash_rejected() {
        let s = body("SafeToNotar Block { slot: 1028070, block_id: EEZ7 }");
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

    // ---- PARSE-06 + COV-01: struct-payload events must reject trailing junk ----

    #[test]
    fn block_notarized_trailing_junk_rejected() {
        let s = body(
            "Block Notarized Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 } (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notar_fallback_trailing_junk_rejected() {
        // COV-01: mirrors PARSE-06 for the notar-fallback path.
        let s = body(
            "Block notar-fallback Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 } (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn safe_to_notar_trailing_junk_rejected() {
        // COV-01: mirrors PARSE-06 for the SafeToNotar path.
        let s = body(
            "SafeToNotar Block { slot: 706643, block_id: \
             EzNUMTPeenPo6d194TAzn3eonj6KJZWd7ynaachXjgqG } (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn finalized_trailing_junk_rejected() {
        // COV-01: mirrors PARSE-06 for the Finalized path. The `fast: BOOL`
        // tail is required and anchored (`$`) by `re_finalized`; a trailing
        // ` (forced)` after `fast: true` must not be silently swallowed.
        let s = body(
            "Finalized Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 } fast: true (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn parent_ready_trailing_junk_rejected() {
        // COV-01: mirrors PARSE-06 for the ParentReady path.
        let s = body(
            "Parent ready 702536 Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 } (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_with_parent_trailing_junk_rejected() {
        // COV-01: mirrors PARSE-06 for the compound Block-with-parent path.
        let s = body(
            "Block Block { slot: 702540, block_id: \
             6mARivgNupeinAsK1sssaP4P8QezwYwW73wb35Njr8jv } parent Block { slot: 702535, \
             block_id: 6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 } (forced)",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn block_notarized_trailing_whitespace_accepted() {
        // Pure trailing whitespace is benign, matching PARSE-02's stance on VotingNotarize.
        let s = body(
            "Block Notarized Block { slot: 702535, block_id: \
             6ynog2g3R2cFhW8zFJbCSHjRkUCTSErP2XrifRJ9Cyx2 }  ",
        );
        assert!(matches!(
            parse_body(&s).unwrap(),
            EventKind::BlockNotarized { slot: 702535, .. }
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

    // ---- DRIFT-01: regression guard against upstream Debug-format drift ----
    //
    // The parser hard-codes the literal prefix `"Block { slot: "` and separator
    // `", block_id: "` that Alpenglow's `#[derive(Debug)]` emits for
    // `struct Block { slot, block_id }`. If a future upstream refactor adds a
    // field, reorders them, or renames one, every affected event silently
    // drops from parse counts — no error, just a metric that goes to zero.
    //
    // This test consumes a verbatim slab of cadabra.log (25 lines of each of
    // the 8 struct-payload event kinds) and asserts every category yields at
    // least one successful parse. The first `cargo test` after a rebase that
    // changes Debug output will fail loudly here.

    #[test]
    fn drift_guard_cadabra_slab_parses_all_event_kinds() {
        use crate::parser::{parse, EventKind, Parsed};

        let slab = include_str!("testdata/cadabra_slab.log");

        let mut n_block = 0u32;
        let mut n_block_notarized = 0u32;
        let mut n_block_notar_fallback = 0u32;
        let mut n_finalized_fast = 0u32;
        let mut n_finalized_slow = 0u32;
        let mut n_safe_to_notar = 0u32;
        let mut n_parent_ready = 0u32;
        let mut n_produce_window = 0u32;
        let mut total_lines = 0u32;
        let mut parsed_lines = 0u32;

        for line in slab.lines() {
            if line.is_empty() {
                continue;
            }
            total_lines += 1;
            let Ok(Parsed::Event(ev)) = parse(line) else {
                continue;
            };
            parsed_lines += 1;
            match ev.kind {
                EventKind::Block { .. } => n_block += 1,
                EventKind::BlockNotarized { .. } => n_block_notarized += 1,
                EventKind::BlockNotarFallback { .. } => n_block_notar_fallback += 1,
                EventKind::Finalized { fast: true, .. } => n_finalized_fast += 1,
                EventKind::Finalized { fast: false, .. } => n_finalized_slow += 1,
                EventKind::SafeToNotar { .. } => n_safe_to_notar += 1,
                EventKind::ParentReady { .. } => n_parent_ready += 1,
                EventKind::ProduceWindow { .. } => n_produce_window += 1,
                _ => {}
            }
        }

        assert!(total_lines > 0, "slab is empty");
        assert_eq!(
            parsed_lines, total_lines,
            "some slab lines failed to parse (upstream Debug format drift?)"
        );
        assert!(n_block > 0, "Block-with-parent count is zero");
        assert!(n_block_notarized > 0, "BlockNotarized count is zero");
        assert!(
            n_block_notar_fallback > 0,
            "BlockNotarFallback count is zero"
        );
        assert!(n_finalized_fast > 0, "Finalized fast count is zero");
        assert!(n_finalized_slow > 0, "Finalized slow count is zero");
        assert!(n_safe_to_notar > 0, "SafeToNotar count is zero");
        assert!(n_parent_ready > 0, "ParentReady count is zero");
        assert!(n_produce_window > 0, "ProduceWindow count is zero");
    }
}
