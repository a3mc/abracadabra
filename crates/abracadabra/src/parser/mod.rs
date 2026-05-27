//! Parser pipeline: turns raw log lines into typed `Event`s.
//!
//! Layout:
//! - `line`         splits the bracketed `[ts LEVEL  module] body` prefix
//! - `votor`        parses `agave_votor::event_handler` lines
//! - `root`         parses `agave_votor::root_utils` lines
//! - `bank`         parses `solana_runtime::bank` lines (`bank frozen: ...`)
//! - `metrics`      parses selected `solana_metrics::metrics` datapoints
//! - `known_issues` parses `solana_core::cluster_slots_service::cluster_slots` etc.

pub mod bank;
pub mod known_issues;
pub mod line;
pub mod metrics;
pub mod root;
pub mod votor;

use std::borrow::Cow;

use regex::Regex;
use thiserror::Error;
use time::OffsetDateTime;

/// Compile a static regex pattern. The pattern strings are programmer-
/// authored in this crate, so a compile failure here is a bug, not a
/// runtime input issue. Used by `votor` and `bank` regex caches.
pub(super) fn must_compile(pattern: &str) -> Regex {
    #[allow(clippy::expect_used)]
    Regex::new(pattern).expect("static regex must compile")
}

pub(super) const fn is_base58_byte(b: u8) -> bool {
    matches!(b, b'1'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'a'..=b'k' | b'm'..=b'z')
}

/// Bounded Base58 hash character class for use inside `regex` patterns.
/// 32..=48 covers real Solana hashes (~43-44 chars after Base58 encoding
/// of 32 bytes) with headroom; the bound prevents unbounded matches on
/// adversarial input.
pub(super) const HASH_CHARS: &str = "[1-9A-HJ-NP-Za-km-z]{32,48}";

/// Bounded decimal digit run for slot / signature_count fields. `u64::MAX`
/// has 20 decimal digits.
pub(super) const SLOT_DIGITS: &str = "[0-9]{1,20}";

/// Length bounds for a Base58 hash string, kept in lockstep with
/// `HASH_CHARS`. The two must match: `validate_base58_hash` is the
/// `strip_prefix`-path counterpart of the regex paths' length bound.
pub(super) const HASH_LEN_MIN: usize = 32;
pub(super) const HASH_LEN_MAX: usize = 48;

/// Length-bounded Base58 validator for `strip_prefix` dispatch paths.
///
/// Combines a Base58-alphabet check with the same `32..=48` length
/// bound the regex paths enforce via `HASH_CHARS`. Use this for any
/// hash captured outside the regex paths so that hash-shape strictness
/// stays symmetric across the parser.
pub(super) fn validate_base58_hash(s: &str) -> Option<&str> {
    if !(HASH_LEN_MIN..=HASH_LEN_MAX).contains(&s.len()) {
        return None;
    }
    if s.bytes().all(is_base58_byte) {
        Some(s)
    } else {
        None
    }
}

/// A parsed log event with its source timestamp.
#[derive(Debug, Clone)]
pub struct Event {
    pub ts: OffsetDateTime,
    pub kind: EventKind,
}

/// The variants of events we extract from the log.
///
/// Pubkey is intentionally not carried on each event: all votor lines in a
/// single-validator log share the same pubkey, which we capture once on the
/// `State` (see model::state, task #13).
#[derive(Debug, Clone)]
pub enum EventKind {
    // -- agave_votor::event_handler --
    Block {
        slot: u64,
        hash: String,
        parent_slot: u64,
        parent_hash: String,
    },
    VotingNotarize {
        slot: u64,
        hash: String,
    },
    VotingFinalize {
        slot: u64,
    },
    VotingSkip {
        slot: u64,
    },
    BlockNotarized {
        slot: u64,
        hash: String,
    },
    BlockNotarFallback {
        slot: u64,
        hash: String,
    },
    Finalized {
        slot: u64,
        hash: String,
        fast: bool,
    },
    FirstShred {
        slot: u64,
    },
    Timeout {
        slot: u64,
    },
    TimeoutCrashedLeader {
        slot: u64,
    },
    SafeToNotar {
        slot: u64,
        hash: String,
    },
    SafeToSkip {
        slot: u64,
    },
    ProduceWindow {
        start: u64,
        end: u64,
        parent_slot: u64,
        parent_hash: String,
    },
    Standstill {
        slot: u64,
    },
    /// `Extending timeouts starting at slot N` — first standstill firing.
    StandstillExtending {
        slot: u64,
    },
    /// `Standstill initially detected at slot=X has ended at slot=Y. Ending timeout extension`.
    StandstillEnded {
        entry_slot: u64,
        exit_slot: u64,
    },
    SetIdentity,
    /// `Refreshing vote {vote:?}` — body details deferred.
    RefreshingVote,

    // -- agave_votor::root_utils --
    SettingRoot {
        slot: u64,
    },
    NewRoot {
        slot: u64,
    },

    // -- solana_runtime::bank --
    /// Truncated for v0.1: full BankHashStats fields parsed lazily later.
    BankFrozen {
        slot: u64,
        hash: String,
        signature_count: u64,
    },

    // -- solana_core::cluster_slots_service::cluster_slots --
    NoEpochMetadata {
        epoch: u64,
    },
    NoEpochInfoForSlot {
        slot: u64,
    },
    UpdatingEpochMetadata {
        epoch: u64,
    },
    EvictingEpochMetadata {
        epoch: u64,
    },
    /// `ClusterSlotsService has stopped because we have finished the alpenglow migration epoch`.
    ClusterSlotsStopped,
    /// `Invalid update call to ClusterSlots, can not roll time backwards!`.
    InvalidClusterSlotsUpdate,

    // -- solana_metrics::metrics (selective) --
    EventHandlerStats {
        event: String,
        count: u64,
        elapsed_us: u64,
    },
    BlockCommitmentCache {
        aggregate_commitment_ms: u64,
        highest_super_majority_root: u64,
        highest_confirmed_slot: u64,
    },
}

/// Outcome of trying to parse a single log line.
#[derive(Debug)]
pub enum Parsed {
    /// A recognised event.
    Event(Event),
    /// WARN/ERROR line from a module with no dedicated event parser —
    /// surfaced upstream so the analyzer can count + group it instead
    /// of silently dropping operational signals.
    Issue {
        ts: time::OffsetDateTime,
        level: line::Level,
        module: String,
        body: String,
    },
    /// Recognised line shape but the body wasn't one we care about.
    Ignored,
    /// Line does not match the bracketed-prefix shape (continuation / wrap).
    Continuation,
}

/// Parser-side error type. Only used for malformed lines we expect to handle;
/// recoverable conditions return `Parsed::Continuation` or `Parsed::Ignored`.
///
/// Variants are payload-free: `runner::run` discards the value (counts only)
/// and no other caller inspects it. Keeping the discriminant lets future
/// diagnostics distinguish failure modes without per-line allocation on the
/// hot path.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed timestamp")]
    Timestamp,
    #[error("malformed slot number")]
    Slot,
    #[error("malformed line prefix")]
    LinePrefix,
}

/// Top-level dispatch. Pure function; no I/O.
///
/// Splits the line via `line::parse_prefix`, then routes the body to the
/// per-module sub-parser based on the module path inside the prefix.
/// Unparsed lines whose level is WARN or ERROR become `Parsed::Issue` so
/// they don't disappear into the `Ignored` count silently.
pub fn parse(raw: &str) -> Result<Parsed, ParseError> {
    let Some(parts) = line::parse_prefix(raw)? else {
        return Ok(Parsed::Continuation);
    };

    let kind = match parts.module {
        "agave_votor::event_handler" => votor::parse_body(parts.body),
        "agave_votor::root_utils" => root::parse_body(parts.body),
        "solana_runtime::bank" => bank::parse_body(parts.body),
        "solana_core::cluster_slots_service::cluster_slots"
        | "solana_core::cluster_slots_service" => known_issues::parse_body(parts.body),
        "solana_metrics::metrics" => metrics::parse_body(parts.body),
        _ => None,
    };

    if let Some(kind) = kind {
        return Ok(Parsed::Event(Event { ts: parts.ts, kind }));
    }

    if matches!(parts.level, line::Level::Warn | line::Level::Error) {
        let truncated = truncate_utf8(parts.body, ISSUE_BODY_MAX_BYTES);
        return Ok(Parsed::Issue {
            ts: parts.ts,
            level: parts.level,
            module: parts.module.to_owned(),
            body: sanitise_issue_body(truncated).into_owned(),
        });
    }

    Ok(Parsed::Ignored)
}

/// Maximum byte length of the body string carried on `Parsed::Issue`.
///
/// `Parsed::Issue` surfaces WARN/ERROR lines from modules without a
/// dedicated event parser. Long bodies (multi-KB BTreeMap dumps, full
/// panic frames, etc.) are truncated on a UTF-8 char boundary so the
/// downstream aggregator's per-issue storage stays bounded.
pub(crate) const ISSUE_BODY_MAX_BYTES: usize = 200;

/// Safely truncate a `&str` to at most `max_bytes`, rounding down to the
/// nearest char boundary so we never split a multi-byte sequence.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

/// Strip dangerous control bytes from an Issue body before it is stored
/// on `Parsed::Issue` (and ultimately on `LogIssueGroup.sample_body`).
///
/// Policy:
/// - `ESC` (0x1B), `DEL` (0x7F), `LF` (0x0A), `CR` (0x0D) are dropped.
///   ESC + CSI would interpret as a CSI sequence on a downstream xterm;
///   CR would cursor-to-column-zero a terminal consuming the body raw.
///   LF is dropped because `BufReader::lines()` already split there, so
///   an embedded LF would only re-appear from a stream where the
///   original `\n` survived. Dropping it keeps the body single-line.
/// - Other ASCII C0 bytes (`0x00..=0x1F` except `\t`) are replaced with
///   `?` so byte-position references in downstream logs stay readable.
/// - `\t` (0x09) is preserved; tables in panic dumps stay aligned.
/// - Multi-byte UTF-8 passes through unchanged.
///
/// Returns `Cow::Borrowed` on the common path where the input is clean.
fn sanitise_issue_body(s: &str) -> Cow<'_, str> {
    if !s.bytes().any(needs_sanitising) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        if c == 0x1B || c == 0x7F || c == 0x0A || c == 0x0D {
            // Drop entirely.
        } else if c < 0x20 && ch != '\t' {
            out.push('?');
        } else {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

#[inline]
const fn needs_sanitising(b: u8) -> bool {
    b == 0x7F || (b < 0x20 && b != b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_event_handler_to_votor() {
        let line = "[2026-05-23T16:00:07.187019566Z INFO  agave_votor::event_handler] \
                    ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu: First shred 1028071";
        let parsed = parse(line).unwrap();
        let Parsed::Event(ev) = parsed else {
            panic!("expected Event, got {parsed:?}");
        };
        assert!(matches!(ev.kind, EventKind::FirstShred { slot: 1028071 }));
    }

    #[test]
    fn routes_root_utils_to_root() {
        let line = "[2026-05-23T16:00:07.339131506Z INFO  agave_votor::root_utils] \
                    ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu: setting root 1028070";
        let Parsed::Event(ev) = parse(line).unwrap() else {
            panic!("expected Event");
        };
        assert!(matches!(ev.kind, EventKind::SettingRoot { slot: 1028070 }));
    }

    #[test]
    fn routes_cluster_slots_to_known_issues() {
        let line = "[2026-05-23T16:00:07.186801076Z ERROR solana_core::cluster_slots_service::cluster_slots] \
                    No epoch_metadata record for epoch 19";
        let Parsed::Event(ev) = parse(line).unwrap() else {
            panic!("expected Event");
        };
        assert!(matches!(ev.kind, EventKind::NoEpochMetadata { epoch: 19 }));
    }

    #[test]
    fn routes_bank_to_bank() {
        let line = "[2026-05-23T16:00:07.257004546Z INFO  solana_runtime::bank] \
                    bank frozen: 1028070 hash: FeYBiF7syZ7SS5vAjrRQNmmqUAkD5TSEMbRN1KfbCdVB \
                    signature_count: 23613 last_blockhash: HnhqoS8KRrp6HbSLt332Ds1UsEGjhmnttDTsQAxm9CGd \
                    capitalization: 502787631450148825";
        let Parsed::Event(ev) = parse(line).unwrap() else {
            panic!("expected Event");
        };
        assert!(matches!(
            ev.kind,
            EventKind::BankFrozen {
                slot: 1028070,
                signature_count: 23613,
                ..
            }
        ));
    }

    #[test]
    fn unknown_module_is_ignored() {
        let line = "[2026-05-23T16:00:07.000Z INFO  solana_quic_client::quic_client] \
                    Timedout sending data 45.152.160.103:6676";
        assert!(matches!(parse(line).unwrap(), Parsed::Ignored));
    }

    #[test]
    fn continuation_line_recognized() {
        let line = "    Address    | Stake";
        assert!(matches!(parse(line).unwrap(), Parsed::Continuation));
    }

    // ---- PARSE-04: truncate_utf8 boundary correctness ----

    #[test]
    fn truncate_utf8_short_input_unchanged() {
        assert_eq!(truncate_utf8("hello", 200), "hello");
    }

    #[test]
    fn truncate_utf8_exact_len() {
        let s = "abc";
        assert_eq!(truncate_utf8(s, 3), "abc");
    }

    #[test]
    fn truncate_utf8_zero_max() {
        // Must never panic; empty result is valid UTF-8.
        assert_eq!(truncate_utf8("hello", 0), "");
    }

    #[test]
    fn truncate_utf8_four_byte_boundary() {
        // 198 ASCII bytes + one 4-byte codepoint (U+1F600 = `0xF0 0x9F 0x98 0x80`),
        // max_bytes = 200. Without boundary rounding, slicing at 200 would split
        // the codepoint. We must back up to 198 (a char boundary).
        let mut s = "a".repeat(198);
        s.push('\u{1F600}');
        let truncated = truncate_utf8(&s, 200);
        assert_eq!(truncated.len(), 198);
        // Round-trip validates UTF-8.
        let _ = truncated.to_owned();
    }

    #[test]
    fn truncate_utf8_pure_ascii_at_max() {
        let s = "x".repeat(500);
        let truncated = truncate_utf8(&s, 200);
        assert_eq!(truncated.len(), 200);
    }

    // ---- PARSE-04: Issue path bounded body ----

    #[test]
    fn issue_path_truncates_to_max() {
        // Synthesize a WARN line from a module without a dedicated parser,
        // with a body longer than ISSUE_BODY_MAX_BYTES.
        let long_body = "X".repeat(500);
        let line =
            format!("[2026-05-23T16:00:07.000000000Z WARN  some_unknown::module] {long_body}");
        let parsed = parse(&line).unwrap();
        let Parsed::Issue { body, .. } = parsed else {
            panic!("expected Parsed::Issue, got {parsed:?}");
        };
        assert!(body.len() <= ISSUE_BODY_MAX_BYTES);
    }

    // ---- PARSE-04: cluster_slots_service routes the same as the sub-module ----

    #[test]
    fn cluster_slots_service_without_subpath_routes_known_issues() {
        // The dispatch table accepts the bare module path; ensure that branch
        // is wired and produces the same event as the fully-qualified form.
        let line = "[2026-05-23T16:00:07.000000000Z ERROR solana_core::cluster_slots_service] \
                    No epoch_metadata record for epoch 19";
        let Parsed::Event(ev) = parse(line).unwrap() else {
            panic!("expected Event for cluster_slots_service bare module");
        };
        assert!(matches!(ev.kind, EventKind::NoEpochMetadata { epoch: 19 }));
    }

    // ---- PARSE-03: bracket-prefixed non-log lines do not inflate parse_errors ----

    #[test]
    fn rust_panic_banner_is_continuation() {
        // Rust panic frames begin with `[` but lack RFC3339 timestamps.
        let line = "[fatal] thread 'main' panicked at 'oops'";
        assert!(matches!(parse(line).unwrap(), Parsed::Continuation));
    }

    #[test]
    fn bom_prefixed_first_line_is_continuation() {
        // BOM + `[` + timestamp. The BOM means the line does not start with
        // `[` at byte 0, so it must route to Continuation (no parse error).
        let line = "\u{FEFF}[2026-05-23T16:00:07.000000000Z INFO  mod::path] body";
        assert!(matches!(parse(line).unwrap(), Parsed::Continuation));
    }

    // ---- PARSE-07: WARN/ERROR fall-through routes voting_utils / root_utils to Issue ----

    #[test]
    fn voting_utils_warn_routes_to_issue() {
        // `voting_utils` is not in the dispatch table; a WARN line from
        // that module must surface as `Parsed::Issue` rather than being
        // silently swallowed by the `Ignored` path.
        let line = "[2026-05-23T16:00:07.000000000Z WARN  agave_votor::voting_utils] \
                    Rank-map vote pubkey mismatch at slot 1028070";
        let parsed = parse(line).unwrap();
        let Parsed::Issue { level, module, .. } = parsed else {
            panic!("expected Parsed::Issue, got {parsed:?}");
        };
        assert_eq!(level, line::Level::Warn);
        assert_eq!(module, "agave_votor::voting_utils");
    }

    #[test]
    fn root_utils_error_routes_to_issue() {
        // `root_utils` IS dispatched, but only the INFO `setting root` /
        // `new root` shapes match. ERROR shapes fall through `root::parse_body`
        // and must route to `Parsed::Issue` via the WARN/ERROR catch.
        let line = "[2026-05-23T16:00:07.000000000Z ERROR agave_votor::root_utils] \
                    failed to record optimistic slot in blockstore: io error";
        let parsed = parse(line).unwrap();
        let Parsed::Issue { level, module, .. } = parsed else {
            panic!("expected Parsed::Issue, got {parsed:?}");
        };
        assert_eq!(level, line::Level::Error);
        assert_eq!(module, "agave_votor::root_utils");
    }

    // ---- PARSE-08: control-byte sanitisation on Issue bodies ----

    #[test]
    fn sanitise_clean_ascii_is_borrowed() {
        // Common path: clean input returns `Cow::Borrowed` (no allocation).
        let s = "plain ASCII body with tab\there";
        let out = sanitise_issue_body(s);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "plain ASCII body with tab\there");
    }

    #[test]
    fn sanitise_strips_esc_and_del() {
        // ESC (0x1B) and DEL (0x7F) are dropped entirely.
        let s = "before\x1b[31mred\x7fafter";
        let out = sanitise_issue_body(s);
        assert_eq!(out, "before[31mredafter");
    }

    #[test]
    fn sanitise_strips_lf_and_cr() {
        // Embedded LF / CR are dropped so the body stays single-line.
        let s = "first\nsecond\rthird";
        let out = sanitise_issue_body(s);
        assert_eq!(out, "firstsecondthird");
    }

    #[test]
    fn sanitise_replaces_other_c0_with_question_mark() {
        // NUL and other C0 bytes (except \t, \n, \r, ESC) become `?`.
        let s = "before\x00\x01\x07after";
        let out = sanitise_issue_body(s);
        assert_eq!(out, "before???after");
    }

    #[test]
    fn sanitise_preserves_tab() {
        let s = "left\tright";
        let out = sanitise_issue_body(s);
        assert_eq!(out, "left\tright");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn sanitise_preserves_multibyte_utf8() {
        // Multi-byte UTF-8 must pass through unchanged.
        let s = "Validator \u{1F600} restarted";
        let out = sanitise_issue_body(s);
        assert_eq!(out, "Validator \u{1F600} restarted");
    }

    #[test]
    fn issue_body_nul_is_sanitised() {
        // End-to-end: an embedded NUL in an Issue body becomes `?`.
        let line = "[2026-05-23T16:00:07.000000000Z WARN  some_unknown::module] \
                    pre\x00post";
        let parsed = parse(line).unwrap();
        let Parsed::Issue { body, .. } = parsed else {
            panic!("expected Parsed::Issue, got {parsed:?}");
        };
        assert_eq!(body, "pre?post");
    }

    #[test]
    fn issue_body_cr_is_stripped() {
        // End-to-end: embedded CR is dropped (would otherwise cursor-to-zero
        // a terminal consuming the body raw via stdout summary).
        let line = "[2026-05-23T16:00:07.000000000Z WARN  some_unknown::module] \
                    pre\rpost";
        let parsed = parse(line).unwrap();
        let Parsed::Issue { body, .. } = parsed else {
            panic!("expected Parsed::Issue, got {parsed:?}");
        };
        assert_eq!(body, "prepost");
    }

    #[test]
    fn issue_body_esc_is_stripped() {
        // End-to-end: embedded ESC is dropped to neutralise CSI sequences.
        let line = "[2026-05-23T16:00:07.000000000Z ERROR some_unknown::module] \
                    pre\x1b[31mpost";
        let parsed = parse(line).unwrap();
        let Parsed::Issue { body, .. } = parsed else {
            panic!("expected Parsed::Issue, got {parsed:?}");
        };
        assert_eq!(body, "pre[31mpost");
    }
}
