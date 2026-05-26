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

use thiserror::Error;
use time::OffsetDateTime;

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
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed timestamp: {0:?}")]
    Timestamp(String),
    #[error("malformed slot number: {0:?}")]
    Slot(String),
    #[error("malformed line prefix: {0:?}")]
    LinePrefix(String),
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
        return Ok(Parsed::Issue {
            ts: parts.ts,
            level: parts.level,
            module: parts.module.to_owned(),
            body: truncate_utf8(parts.body, 200).to_owned(),
        });
    }

    Ok(Parsed::Ignored)
}

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
}
