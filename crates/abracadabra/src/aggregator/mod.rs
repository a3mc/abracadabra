//! State mutator: applies each parsed `Event` to the running `State`.
//!
//! Splitting this from `parser` keeps the parser pure (Event-yielding only)
//! and the aggregator focused on bookkeeping. The TUI can read State without
//! understanding the event grammar.

use std::collections::HashSet;

use time::OffsetDateTime;

use crate::model::alerts::{Alert, AlertKind, Severity};
use crate::model::slot::{CanonicalSkipEvidence, SkipClassification};
use crate::model::state::{LogIssueGroup, State, MAX_GROUP_TIMESTAMPS};
use crate::parser::line::Level;
use crate::parser::{Event, EventKind};

/// Maximum tolerated `end - start` span on an `EventKind::ProduceWindow`.
///
/// Solana's `NUM_CONSECUTIVE_LEADER_SLOTS = 4` makes the real span exactly
/// `3` (i.e. `end_slot - start_slot == 3`, four inclusive slots). The cap
/// is `16` — four times the spec — so a window with a damaged digit still
/// fits but a truncated `u64::MAX` end_slot does not. Rejected events are
/// counted in `OverallStats::malformed_produce_window` and the slot loop
/// is skipped entirely; otherwise a single malformed log line could force
/// the aggregator to materialise exabytes of `SlotRecord`s.
pub const MAX_LEADER_WINDOW_SPAN: u64 = 16;

/// Apply a single event to `state`.
///
/// Single-shot per `Event`: replaying the same event onto the same
/// `State` would duplicate inline-pushed alerts (`StandstillObserved`,
/// `IdentityChanged`, `ClusterSlotsShutdownObserved`) because those are
/// emitted unconditionally during ingest rather than reconstructed by
/// `analyze`. The single production caller (`runner::run`) honours this;
/// future callers must too.
pub fn ingest(state: &mut State, event: Event) {
    state.observe_ts(event.ts);

    match event.kind {
        EventKind::Block {
            slot,
            hash,
            parent_slot,
            parent_hash,
        } => {
            let rec = state.slot_mut(slot);
            rec.block_id.get_or_insert(hash);
            rec.parent.get_or_insert((parent_slot, parent_hash));
            rec.block_emitted_at.get_or_insert(event.ts);
        }
        EventKind::FirstShred { slot } => {
            state.slot_mut(slot).first_shred_at.get_or_insert(event.ts);
            state.overall.first_shreds = state.overall.first_shreds.saturating_add(1);
        }
        EventKind::VotingNotarize { slot, hash } => {
            let rec = state.slot_mut(slot);
            rec.voted_notarize_at.get_or_insert(event.ts);
            rec.block_id.get_or_insert(hash);
            state.overall.votes_notarize = state.overall.votes_notarize.saturating_add(1);
        }
        EventKind::VotingFinalize { slot } => {
            state
                .slot_mut(slot)
                .voted_finalize_at
                .get_or_insert(event.ts);
            state.overall.votes_finalize = state.overall.votes_finalize.saturating_add(1);
        }
        EventKind::VotingSkip { slot } => {
            state.slot_mut(slot).voted_skip_at.get_or_insert(event.ts);
            state.overall.votes_skip = state.overall.votes_skip.saturating_add(1);
        }
        EventKind::BlockNotarized { slot, hash } => {
            let rec = state.slot_mut(slot);
            rec.block_notarized_at.get_or_insert(event.ts);
            rec.block_id.get_or_insert(hash);
            state.overall.block_notarized_count =
                state.overall.block_notarized_count.saturating_add(1);
        }
        EventKind::BlockNotarFallback { slot, hash } => {
            let rec = state.slot_mut(slot);
            rec.notar_fallback_at.get_or_insert(event.ts);
            rec.block_id.get_or_insert(hash);
            state.overall.block_notar_fallback_count =
                state.overall.block_notar_fallback_count.saturating_add(1);
        }
        EventKind::Finalized { slot, hash, fast } => {
            let rec = state.slot_mut(slot);
            rec.finalized_at.get_or_insert(event.ts);
            rec.fast_finalize.get_or_insert(fast);
            rec.block_id.get_or_insert(hash);
            if fast {
                state.overall.finalized_fast = state.overall.finalized_fast.saturating_add(1);
            } else {
                state.overall.finalized_slow = state.overall.finalized_slow.saturating_add(1);
            }
        }
        EventKind::SettingRoot { slot } => {
            state.slot_mut(slot).setting_root_at.get_or_insert(event.ts);
            state.overall.setting_root_count = state.overall.setting_root_count.saturating_add(1);
        }
        EventKind::NewRoot { slot } => {
            state.slot_mut(slot).new_root_at.get_or_insert(event.ts);
            state.overall.new_root_count = state.overall.new_root_count.saturating_add(1);
        }
        EventKind::Timeout { slot } => {
            state.slot_mut(slot).timeout_at.get_or_insert(event.ts);
            state.overall.timeouts = state.overall.timeouts.saturating_add(1);
        }
        EventKind::TimeoutCrashedLeader { slot } => {
            state
                .slot_mut(slot)
                .timeout_crashed_leader_at
                .get_or_insert(event.ts);
            state.overall.timeout_crashed_leaders =
                state.overall.timeout_crashed_leaders.saturating_add(1);
        }
        EventKind::SafeToNotar { slot, hash } => {
            let rec = state.slot_mut(slot);
            rec.safe_to_notar_at.get_or_insert(event.ts);
            rec.block_id.get_or_insert(hash);
            state.overall.safe_to_notar = state.overall.safe_to_notar.saturating_add(1);
        }
        EventKind::SafeToSkip { slot } => {
            state.slot_mut(slot).safe_to_skip_at.get_or_insert(event.ts);
            state.overall.safe_to_skip = state.overall.safe_to_skip.saturating_add(1);
        }
        EventKind::ProduceWindow { start, end, .. } => {
            // Corruption guard: reject windows whose span exceeds
            // `MAX_LEADER_WINDOW_SPAN`. See the constant's docstring for
            // the rationale (defends against `end = u64::MAX` from a
            // truncated log line that would otherwise materialise an
            // unbounded number of `SlotRecord`s).
            if end < start || end.saturating_sub(start) > MAX_LEADER_WINDOW_SPAN {
                state.overall.malformed_produce_window =
                    state.overall.malformed_produce_window.saturating_add(1);
                return;
            }
            for s in start..=end {
                state.slot_mut(s).we_are_leader = true;
            }
            state.overall.produce_windows = state.overall.produce_windows.saturating_add(1);
            state.overall.produce_window_timestamps.push(event.ts);
        }
        EventKind::Standstill { slot } => {
            state.overall.standstill_events = state.overall.standstill_events.saturating_add(1);
            // Dedup by at_slot: a sustained cluster halt fires
            // `Standstill {slot}` every ~10s with the same slot. Without
            // this merge a 3-hour halt produces ~1000 separate alerts.
            // Mirrors the `LogPattern` count convention.
            if let Some(&idx) = state.overall.standstill_alert_indices.get(&slot) {
                if let Some(alert) = state.alerts.get_mut(idx) {
                    if let AlertKind::StandstillObserved {
                        ref mut count,
                        ref mut last_at,
                        ..
                    } = alert.kind
                    {
                        *count = count.saturating_add(1);
                        *last_at = event.ts;
                    }
                }
            } else {
                let idx = state.alerts.len();
                state.alerts.push(Alert::new(
                    Severity::Warn,
                    event.ts,
                    AlertKind::StandstillObserved {
                        at_slot: slot,
                        count: 1,
                        last_at: event.ts,
                    },
                    format!("Standstill firing at finalized slot {slot}"),
                ));
                state.overall.standstill_alert_indices.insert(slot, idx);
            }
        }
        EventKind::StandstillExtending { slot } => {
            state.overall.standstill_extending_events =
                state.overall.standstill_extending_events.saturating_add(1);
            // First Extending in a stuck period anchors the entry slot.
            // Repeated extends inside the same period do not re-anchor.
            if state.overall.open_standstill_entry.is_none() {
                state.overall.open_standstill_entry = Some(slot);
            }
        }
        EventKind::StandstillEnded {
            entry_slot,
            exit_slot,
        } => {
            state.overall.standstill_ended_events =
                state.overall.standstill_ended_events.saturating_add(1);
            state
                .overall
                .standstill_ranges
                .push((entry_slot, exit_slot));
            state.overall.open_standstill_entry = None;
        }
        EventKind::RefreshingVote => {
            state.overall.refreshing_votes = state.overall.refreshing_votes.saturating_add(1);
        }
        EventKind::TriggeringParentReady { .. } => {
            // Empirically (~3,800/hr in steady-state) this fires ~twice per
            // leader window as the finalization chain catches up positions
            // %4=1 and %4=2. Not a recovery signal in normal operation —
            // we count it but do not surface per-slot.
            // See docs/alpenglow/07-safety-machinery.md.
            state.overall.parent_ready_recoveries =
                state.overall.parent_ready_recoveries.saturating_add(1);
        }
        EventKind::SetIdentity => {
            // Operator rotated validator identity — INFO timeline anchor.
            // Pushed inline (not via analyze) so the Alert.at carries
            // the actual identity-change timestamp rather than the log's
            // last-seen time.
            state.alerts.push(Alert::new(
                Severity::Info,
                event.ts,
                AlertKind::IdentityChanged,
                "Operator rotated validator identity (Set identity event)".to_owned(),
            ));
        }
        EventKind::BankFrozen {
            slot,
            signature_count,
            ..
        } => {
            state.overall.bank_frozen_count = state.overall.bank_frozen_count.saturating_add(1);
            // signature_count = signed tx count in the bank (user txs +
            // vote txs). Saved on the SlotRecord; aggregated per
            // time-bucket in `model::buckets` for the tx-pressure card.
            state
                .slot_mut(slot)
                .signature_count
                .get_or_insert(signature_count);
        }
        EventKind::NoEpochMetadata { epoch } => {
            state.overall.no_epoch_metadata = state.overall.no_epoch_metadata.saturating_add(1);
            record_log_pattern(
                state,
                event.ts,
                Severity::Critical,
                CLUSTER_SLOTS_MODULE,
                || format!("No epoch_metadata record for epoch {epoch}"),
            );
        }
        EventKind::NoEpochInfoForSlot { slot } => {
            state.overall.no_epoch_info_for_slot =
                state.overall.no_epoch_info_for_slot.saturating_add(1);
            record_log_pattern(
                state,
                event.ts,
                Severity::Critical,
                CLUSTER_SLOTS_MODULE,
                || format!("No epoch info for slot {slot}"),
            );
        }
        EventKind::UpdatingEpochMetadata { .. } => {
            // INFO-level — counter-only, no alert.
            state.overall.updating_epoch_metadata =
                state.overall.updating_epoch_metadata.saturating_add(1);
        }
        EventKind::EvictingEpochMetadata { .. } => {
            // INFO-level — counter-only, no alert.
            state.overall.evicting_epoch_metadata =
                state.overall.evicting_epoch_metadata.saturating_add(1);
        }
        EventKind::ClusterSlotsStopped => {
            state.overall.cluster_slots_service_stopped = true;
            state.alerts.push(Alert::new(
                Severity::Info,
                event.ts,
                AlertKind::ClusterSlotsShutdownObserved,
                "ClusterSlotsService has stopped — validator entered FullAlpenglowEpoch".to_owned(),
            ));
        }
        EventKind::InvalidClusterSlotsUpdate => {
            state.overall.invalid_cluster_slots_update =
                state.overall.invalid_cluster_slots_update.saturating_add(1);
            record_log_pattern(
                state,
                event.ts,
                Severity::Warn,
                CLUSTER_SLOTS_MODULE,
                || "Invalid update call to ClusterSlots, can not roll time backwards!".to_owned(),
            );
        }
        EventKind::EventHandlerStats { .. } | EventKind::BlockCommitmentCache { .. } => {
            // v0.3: parse selective datapoints into model. No-op for v0.1.
        }
    }
}

/// Ingest one WARN/ERROR line from an unparsed module.
///
/// Aggregated by `(severity, module)` so a thousand identical lines
/// collapse to one `LogIssueGroup` entry with `count = 1000`. The first
/// body sample is retained verbatim (already truncated upstream); the
/// last-seen timestamp and the count update on every hit.
pub fn ingest_issue(
    state: &mut State,
    ts: OffsetDateTime,
    level: Level,
    module: String,
    body: String,
) {
    state.observe_ts(ts);
    let severity = match level {
        Level::Error => Severity::Critical,
        Level::Warn => Severity::Warn,
        _ => return, // Only WARN/ERROR are surfaced as issues.
    };
    // Body is already owned here, so the closure is just a move — no
    // alloc deferral benefit, but uniform with the call-site signature.
    record_log_pattern(state, ts, severity, &module, move || body);
}

/// Add (or update) a `LogIssueGroup` keyed by `(severity, module)`.
///
/// Used both by `ingest_issue` (parser-failure WARN/ERROR lines from
/// unknown modules) and by structured `EventKind` handlers for modules
/// with a documented investigation (e.g. `NoEpochMetadata`). Both paths
/// share the same `(severity, module)` bucket, so the alerts panel
/// surfaces one row per module with a unified count.
///
/// `body` is a `FnOnce() -> String` so the caller's body string is only
/// materialised on the insert path. The update path is the hot one
/// (~137k hits per 21h log on `cluster_slots`); deferring the alloc
/// behind a closure avoids that many wasted `String` constructions.
///
/// The `timestamps` vector is capped at `MAX_GROUP_TIMESTAMPS`. Overflow
/// timestamps are counted in `timestamps_dropped` but not stored; `count`
/// always reflects the true total. See `LogIssueGroup` for the rationale.
///
/// Note (intentional aggregation): for `cluster_slots`, `count` may
/// exceed `state.overall.no_epoch_metadata` when unparsed WARN/ERROR
/// lines from the same module also land here. The structured per-event
/// counter measures only the structured branch; the group `count`
/// measures structured + unstructured combined.
//
// [REVIEW] `module.to_owned()` still allocates a fresh `String` per
// entry probe. Removing it would require `hashbrown::raw_entry_mut` or a
// `Borrow`-impl newtype; measured negligible against I/O at current call
// rates (~137k/log). Body allocation is deferred via the closure
// argument so the update path pays no `String` cost.
fn record_log_pattern<F: FnOnce() -> String>(
    state: &mut State,
    ts: OffsetDateTime,
    severity: Severity,
    module: &str,
    body: F,
) {
    let entry = state
        .overall
        .log_issues
        .entry((severity, module.to_owned()))
        .or_insert_with(|| LogIssueGroup {
            severity,
            module: module.to_owned(),
            count: 0,
            first_at: ts,
            last_at: ts,
            sample_body: body(),
            timestamps: Vec::new(),
            timestamps_dropped: 0,
        });
    entry.count = entry.count.saturating_add(1);
    entry.last_at = ts;
    if entry.timestamps.len() < MAX_GROUP_TIMESTAMPS {
        entry.timestamps.push(ts);
    } else {
        entry.timestamps_dropped = entry.timestamps_dropped.saturating_add(1);
    }
}

const CLUSTER_SLOTS_MODULE: &str = "solana_core::cluster_slots_service::cluster_slots";

/// Stage 1 canonical-skip classifier (pure log; no RPC).
///
/// For each slot where we cast a Skip vote, prove the slot was
/// canonical from log alone via:
///
///   (a) **direct evidence** — we observed a `Finalized` event for this
///       slot ourselves. The cluster reached a finalization cert here.
///   (b) **ancestry evidence** — some descendant of this slot is
///       `Finalized` AND the parent chain from that descendant reaches
///       this slot. The cluster placed this slot on the rooted chain.
///
/// Otherwise the slot is `Indeterminate` — could be a right-skip
/// (cluster also skipped) or an unverified canonical skip. Stage 2
/// (RPC enrichment, future work) would tighten this further by
/// querying `getBlocks` for ground truth.
///
/// Empirical validation: on 5 logs spanning ~5 days, this classifier
/// matched RPC ground truth 100% on the verifiable subset. See
/// `audit/2026-05-27-tui-vs-alpenglow/TRIAGE.md` section D1.
///
/// Must be called once after `ingest` has processed the full stream.
/// `analyze` invokes this; external callers should not call it again.
pub fn classify_skips(state: &mut State) {
    // Step 1: collect the set of slots we directly observed `Finalized` for.
    let finalized: HashSet<u64> = state
        .slots
        .iter()
        .filter_map(|(s, r)| r.finalized_at.is_some().then_some(*s))
        .collect();

    // Step 2: walk parent pointers backward from each finalized slot to
    // collect the full ancestor set. Loop guard via the visited check
    // (`insert` returns false if already in) prevents pathological
    // cycles from a corrupted parent map.
    let mut ancestors: HashSet<u64> = HashSet::new();
    for &fin in &finalized {
        let mut cur = fin;
        while let Some((parent_slot, _)) = state.slots.get(&cur).and_then(|r| r.parent.as_ref()) {
            let p = *parent_slot;
            if !ancestors.insert(p) {
                break;
            }
            cur = p;
        }
    }

    // Step 3: classify each slot record AND collect unique-slot counts
    // for the headline KPIs (since per-event counters in `OverallStats`
    // double-count canonical-skip slots which appear in both the
    // `votes_skip` and `finalized_*` buckets).
    let mut canon_direct: u64 = 0;
    let mut canon_ancestry: u64 = 0;
    let mut indeterminate: u64 = 0;
    let mut finalized_slot_count: u64 = 0;
    let mut skipped_slot_count: u64 = 0;
    let mut pending_slot_count: u64 = 0;
    for (slot, rec) in &mut state.slots {
        if rec.voted_skip_at.is_none() {
            rec.skip_classification = SkipClassification::NotSkipped;
        } else {
            skipped_slot_count = skipped_slot_count.saturating_add(1);
            rec.skip_classification = if rec.finalized_at.is_some() {
                canon_direct = canon_direct.saturating_add(1);
                SkipClassification::CanonicalSkip(CanonicalSkipEvidence::DirectFinalize)
            } else if ancestors.contains(slot) {
                canon_ancestry = canon_ancestry.saturating_add(1);
                SkipClassification::CanonicalSkip(CanonicalSkipEvidence::Ancestry)
            } else {
                indeterminate = indeterminate.saturating_add(1);
                SkipClassification::Indeterminate
            };
        }
        if rec.finalized_at.is_some() {
            finalized_slot_count = finalized_slot_count.saturating_add(1);
        }
        if rec.finalized_at.is_none() && rec.voted_skip_at.is_none() {
            pending_slot_count = pending_slot_count.saturating_add(1);
        }
    }
    state.overall.canonical_skips_direct = canon_direct;
    state.overall.canonical_skips_ancestry = canon_ancestry;
    state.overall.indeterminate_skips = indeterminate;
    state.overall.finalized_slot_count = finalized_slot_count;
    state.overall.skipped_slot_count = skipped_slot_count;
    state.overall.pending_slot_count = pending_slot_count;
}

/// Close off an unmatched `StandstillExtending` left open at end-of-stream
/// by pushing `(entry, max_slot_seen)` into `standstill_ranges`. The log
/// may have been cut mid-standstill, or the validator may still be in one
/// at capture time; either way the range needs an upper bound.
fn close_open_standstill(state: &mut State) {
    let Some(entry) = state.overall.open_standstill_entry.take() else {
        return;
    };
    let exit = state.slots.keys().copied().max().unwrap_or(entry);
    state.overall.standstill_ranges.push((entry, exit));
}

/// Populate `timeout_crashed_leaders_outside_standstill` by iterating
/// `SlotRecord`s and counting `timeout_crashed_leader_at`-bearing slots
/// whose slot number is not inside any standstill range.
///
/// Ranges expected to be small (single digits in typical logs), so a
/// linear scan per TCL slot is fine.
fn compute_tcl_outside_standstill(state: &mut State) {
    let ranges = &state.overall.standstill_ranges;
    if ranges.is_empty() {
        state.overall.timeout_crashed_leaders_outside_standstill =
            state.overall.timeout_crashed_leaders;
        return;
    }
    let mut count: u64 = 0;
    for (slot, rec) in &state.slots {
        if rec.timeout_crashed_leader_at.is_none() {
            continue;
        }
        let in_standstill = ranges
            .iter()
            .any(|(entry, exit)| *slot >= *entry && *slot <= *exit);
        if !in_standstill {
            count = count.saturating_add(1);
        }
    }
    state.overall.timeout_crashed_leaders_outside_standstill = count;
}

/// Derived alerts computed after the stream has been fully ingested.
///
/// Emits one `LogPattern` alert per `(severity, module)` group and a
/// single `LocalLeaderSummary` INFO alert when the validator was
/// scheduled to lead any windows. The cluster-slots loose-end pattern
/// surfaces via the LogPattern alert for
/// `solana_core::cluster_slots_service` — see
/// `docs/alpenglow/investigations/01-cluster-slots-loose-end.md` for
/// the analytical context.
pub fn analyze(state: &mut State) {
    // Guard against duplicate emission. `surface_*` push unconditionally;
    // calling `analyze` twice would double every derived alert. Single
    // caller today (`runner`), but a regression would silently inflate
    // counts in the TUI.
    debug_assert!(
        !state.alerts.iter().any(|a| matches!(
            a.kind,
            AlertKind::LogPattern { .. } | AlertKind::LocalLeaderSummary { .. }
        )),
        "analyze() must run at most once per State",
    );
    classify_skips(state);
    close_open_standstill(state);
    compute_tcl_outside_standstill(state);
    surface_log_pattern_alerts(state);
    surface_local_leader_summary(state);
    // Establish the post-analyze invariant on `state.alerts`: Critical
    // before Warn before Info, stream order within a severity bucket, a
    // stable kind discriminant as last tiebreaker. The TUI alerts panel
    // title advertises "CRIT first" and consumers index into the vector
    // by `app.alert_scroll` — both rely on this ordering.
    state.alerts.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.at.cmp(&b.at))
            .then_with(|| alert_kind_rank(&a.kind).cmp(&alert_kind_rank(&b.kind)))
    });
}

/// Stable integer rank per `AlertKind` variant. Used only as the last
/// tiebreaker in the `state.alerts` sort — the absolute values do not
/// leak into any user-visible artefact, but the relative order must not
/// change across runs (HashMap iteration order on the `LogPattern`
/// source would otherwise drift).
const fn alert_kind_rank(kind: &AlertKind) -> u8 {
    match kind {
        AlertKind::LogPattern { .. } => 0,
        AlertKind::StandstillObserved { .. } => 1,
        AlertKind::ClusterSlotsShutdownObserved => 2,
        AlertKind::IdentityChanged => 3,
        AlertKind::LocalLeaderSummary { .. } => 4,
    }
}

/// Emit a single INFO alert summarising the validator's leader windows.
/// The Alerts panel renders a sparkline from
/// `state.overall.produce_window_timestamps` showing when leader
/// windows fell across the log — useful for "was I leader during the
/// outage at 04:42?" queries.
fn surface_local_leader_summary(state: &mut State) {
    let window_count = state.overall.produce_windows;
    if window_count == 0 {
        return;
    }
    let slot_count: u64 = state.slots.values().filter(|s| s.we_are_leader).count() as u64;
    // `window_count > 0` implies at least one `ProduceWindow` event was
    // ingested, which pushes its timestamp; so the first-timestamp path
    // always wins. The `time_range.lo` arm guards a future refactor that
    // separates the counter from the timestamp list — it stays
    // deterministic (no `now_utc()` wall-clock leak into output). If
    // both are missing we skip the alert.
    let at = match state.overall.produce_window_timestamps.first().copied() {
        Some(ts) => ts,
        None => match state.file_meta.time_range {
            Some((lo, _)) => lo,
            None => return,
        },
    };
    // Show the math so the relationship is unambiguous in the alerts
    // panel preview: a "window" is a 4-slot burst assigned to one
    // leader (Solana `NUM_CONSECUTIVE_LEADER_SLOTS = 4`).
    let description = format!(
        "Local validator was leader: {slot_count} slots = {window_count} \
         windows × 4 slots/window"
    );
    state.alerts.push(Alert::new(
        Severity::Info,
        at,
        AlertKind::LocalLeaderSummary {
            slot_count,
            window_count,
        },
        description,
    ));
}

/// Emit one alert per `(severity, module)` group of unparsed WARN/ERROR
/// lines. Sorted Critical-first, then by count, so the alerts panel
/// reads worst-first.
fn surface_log_pattern_alerts(state: &mut State) {
    if state.overall.log_issues.is_empty() {
        return;
    }
    // Projection: copy only the scalar fields plus owned strings for
    // `module` and `sample_body`. `LogIssueGroup.timestamps` is *not*
    // copied — the TUI looks the canonical group up via
    // `State::log_issues_get` when it needs the sparkline. Cloning the
    // full group here would copy up to ~16 MB per group at
    // `MAX_GROUP_TIMESTAMPS` for data this function never reads.
    let mut groups: Vec<LogPatternView> = state
        .overall
        .log_issues
        .values()
        .map(|g| LogPatternView {
            severity: g.severity,
            count: g.count,
            first_at: g.first_at,
            module: g.module.clone(),
            sample_body: g.sample_body.clone(),
        })
        .collect();
    // Deterministic order: severity desc, then count desc, then module
    // asc, then first_at asc. The two tiebreakers below the count guard
    // against HashMap iteration order leaking into the TUI cursor
    // position — two groups with the same (severity, count) must always
    // emit in the same order across runs.
    groups.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.module.cmp(&b.module))
            .then_with(|| a.first_at.cmp(&b.first_at))
    });
    for g in groups {
        let label = match g.severity {
            Severity::Critical => "ERROR",
            Severity::Warn => "WARN",
            Severity::Info => "INFO",
        };
        let plural = if g.count == 1 { "" } else { "s" };
        let description = format!(
            "{label} {} ({} occurrence{plural}): {}",
            g.module, g.count, g.sample_body,
        );
        state.alerts.push(Alert::new(
            g.severity,
            g.first_at,
            AlertKind::LogPattern {
                severity: g.severity,
                module: g.module,
                count: g.count,
            },
            description,
        ));
    }
}

/// Sorting/projection view over `LogIssueGroup` for
/// `surface_log_pattern_alerts`. Carries only the fields the surface
/// reads; in particular it omits `timestamps`, which can be up to
/// `MAX_GROUP_TIMESTAMPS * 16` bytes and is never inspected here.
struct LogPatternView {
    severity: Severity,
    count: u64,
    first_at: OffsetDateTime,
    module: String,
    sample_body: String,
}

#[cfg(test)]
mod tests;
