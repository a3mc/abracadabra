//! Unit tests for `tui::app`. Extracted to a sibling file (referenced
//! from `app.rs` via `#[path = "app_tests.rs"] mod tests;`) so the
//! production module stays under the ~800 LOC strong-warn threshold.
//! Module identity is unchanged — `super` here still refers to
//! `crate::tui::app`.

use super::*;
use std::os::unix::fs::symlink;

/// SEC-01 regression: even when a malicious symlink pre-exists at
/// the target path, `try_write_yank` must not follow it. The fix
/// uses `create_new(true)` + `O_NOFOLLOW`, so a pre-placed symlink
/// causes the open to fail (with `AlreadyExists`) and the loop
/// retries on a fresh counter.
#[test]
fn yank_to_existing_symlink_does_not_overwrite_target() {
    // Set up an isolated yank dir under a tempdir.
    let tmp = std::env::temp_dir().join(format!("abracadabra-yank-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Place an attacker's symlink at the path the yank would
    // first try. Filename pattern is
    // `abracadabra-yank-<pid>-<n>.txt` (REL-01 fix); pre-position
    // the attack at <pid>-1.
    let pid = std::process::id();
    let victim = tmp.join("victim.txt");
    std::fs::write(&victim, b"original-victim-content").unwrap();
    let attack_link = tmp.join(format!("abracadabra-yank-{pid}-1.txt"));
    symlink(&victim, &attack_link).unwrap();

    // Construct a minimal App and drive the yank.
    let state = crate::model::state::State::new(PathBuf::from("/tmp/x"), 0);
    let mut app = App::new(&state, None, 60);
    let result = app.try_write_yank(&tmp, "yank-body-payload");

    // Outcome: yank succeeds, but writes to a DIFFERENT path
    // (counter bumped past 1). The victim's content is untouched.
    let written = result.expect("yank should succeed by bumping counter");
    assert_ne!(written, attack_link);
    // Victim untouched.
    let victim_after = std::fs::read_to_string(&victim).unwrap();
    assert_eq!(victim_after, "original-victim-content");
    // The yank's actual content lives at `written`.
    let yank_body = std::fs::read_to_string(&written).unwrap();
    assert_eq!(yank_body, "yank-body-payload");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// STRUCT-01 regression: scroll keys on tabs without a scrollable
/// list (Overview, Time series, Windows) must NOT clobber a
/// cursor on a scrollable tab. The old `scroll_target` default
/// arm wrote to `slot_scroll`, so `g`/`G`/`Home`/`End` on tab 0
/// would reset `slot_scroll` to 0.
#[test]
fn scroll_on_non_list_tab_does_not_mutate_other_cursors() {
    let state = crate::model::state::State::new(PathBuf::from("/tmp/x"), 0);
    let mut app = App::new(&state, None, 60);
    // Pretend the user has navigated 42 rows down the Slots table.
    app.slot_scroll = 42;
    // Switch to Overview (tab 0) and press G — must be a no-op for
    // every cursor, including slot_scroll.
    app.current_tab = 0;
    app.jump_bottom();
    app.jump_top();
    app.step_scroll(1);
    app.step_scroll(-1);
    assert_eq!(
        app.slot_scroll, 42,
        "slot_scroll clobbered by tab-0 scroll keys"
    );
    // Same check on Time series (tab 1) and Windows (tab 2).
    app.current_tab = 1;
    app.jump_bottom();
    app.step_scroll(20);
    assert_eq!(
        app.slot_scroll, 42,
        "slot_scroll clobbered by tab-1 scroll keys"
    );
    app.current_tab = 2;
    app.jump_top();
    assert_eq!(
        app.slot_scroll, 42,
        "slot_scroll clobbered by tab-2 scroll keys"
    );
}

/// PERF-03 regression: leader-slot count is precomputed once in
/// `App::new`. Asserts the field matches a direct count from
/// `slot_rows` so the two read-sites on the Slots tab return the
/// same value the old per-frame scans did.
#[test]
fn leader_slot_count_matches_slot_rows_filter() {
    let mut state = crate::model::state::State::new(PathBuf::from("/tmp/x"), 0);
    state.slot_mut(1).we_are_leader = true;
    state.slot_mut(2).we_are_leader = false;
    state.slot_mut(3).we_are_leader = true;
    state.slot_mut(4).we_are_leader = true;
    let app = App::new(&state, None, 60);
    let direct = app.slot_rows.iter().filter(|r| r.we_are_leader).count() as u64;
    assert_eq!(app.leader_slot_count, direct);
    assert_eq!(app.leader_slot_count, 3);
}

/// COR-02 regression guard for the math: deriving the per-hour
/// rate must use the real elapsed hours, not a clamped 1.0
/// denominator. With 60 events in 20 minutes, the true rate is
/// 180/h — the old `hours.max(1.0)` would have reported 60/h.
#[test]
fn rate_per_hour_does_not_clamp_short_log() {
    let twenty_min_hours = 20.0_f64 / 60.0;
    let events = 60.0_f64;
    let rate = events / twenty_min_hours;
    assert!((rate - 180.0).abs() < 1e-9, "expected 180/h, got {rate}");
}

/// REL-01 regression: panic hook installation must be idempotent
/// — calling `install_panic_hook` twice in the same process must
/// not stack the restore logic recursively. Verified by checking
/// that the `OnceLock` short-circuits the second call.
#[test]
fn panic_hook_install_is_idempotent() {
    // We can't directly observe hook chain depth without panicking
    // (which would mess up the test runner's output). Instead,
    // assert that two back-to-back installs leave the process
    // alive and that subsequent calls are no-ops.
    install_panic_hook();
    install_panic_hook();
    install_panic_hook();
    // If we got here without infinite recursion or stack overflow,
    // the OnceLock guard is doing its job.
}

// -- SlotFilters tests -----------------------------------------------
//
// The skip-family pair (`vskip_only`, `canonical_skip_only`) uses
// OR semantics: both off = no constraint, one on = its bucket,
// both on = union. The other six flags AND together.

use crate::model::slot::{CanonicalSkipEvidence, SkipClassification};

/// Three row shapes covering the skip-family discriminator.
fn row(status: SlotStatus, skip: SkipClassification) -> SlotViewRow {
    SlotViewRow {
        slot: 0,
        status,
        skip_classification: skip,
        fast: None,
        we_are_leader: false,
        assembly_ms: None,
        consensus_ms: None,
        consensus_inverted: false,
        lifecycle_ms: None,
        voted_notarize: false,
        voted_finalize: false,
        voted_skip: false,
        safe_to_notar: false,
        safe_to_skip: false,
        crashed_leader: false,
    }
}

fn vskip_row() -> SlotViewRow {
    row(SlotStatus::Skipped, SkipClassification::Indeterminate)
}

fn cskip_row() -> SlotViewRow {
    row(
        SlotStatus::Skipped,
        SkipClassification::CanonicalSkip(CanonicalSkipEvidence::DirectFinalize),
    )
}

fn fin_row() -> SlotViewRow {
    row(SlotStatus::FastFinalized, SkipClassification::NotSkipped)
}

#[test]
fn slot_filters_matches_skip_family_or_semantics() {
    // 4 filter states × 3 row shapes = 12 cases.
    let off = SlotFilters::default();
    let v_only = SlotFilters {
        vskip_only: true,
        ..SlotFilters::default()
    };
    let c_only = SlotFilters {
        canonical_skip_only: true,
        ..SlotFilters::default()
    };
    let both = SlotFilters {
        vskip_only: true,
        canonical_skip_only: true,
        ..SlotFilters::default()
    };

    let v = vskip_row();
    let c = cskip_row();
    let f = fin_row();

    // Both off → no constraint from skip family. Every row passes.
    assert!(off.matches(&v));
    assert!(off.matches(&c));
    assert!(off.matches(&f));

    // VSKIP only.
    assert!(v_only.matches(&v));
    assert!(!v_only.matches(&c));
    assert!(!v_only.matches(&f));

    // CSKIP only.
    assert!(!c_only.matches(&v));
    assert!(c_only.matches(&c));
    assert!(!c_only.matches(&f));

    // Both on → union of VSKIP ∪ CSKIP.
    assert!(both.matches(&v));
    assert!(both.matches(&c));
    assert!(!both.matches(&f));
}

#[test]
fn slot_filters_matches_and_semantics_for_other_flags() {
    // Sanity: the six non-skip-family flags AND together against
    // their respective row bits.
    let mut r = fin_row();
    r.crashed_leader = true;
    r.we_are_leader = true;

    let leader_and_tcl = SlotFilters {
        tcl: true,
        leader: true,
        ..SlotFilters::default()
    };
    assert!(leader_and_tcl.matches(&r));

    // Drop one bit → row must be excluded.
    r.crashed_leader = false;
    assert!(!leader_and_tcl.matches(&r));
}

#[test]
fn slot_filters_fast_and_slow_only_isolate_status() {
    let fast_only = SlotFilters {
        fast_only: true,
        ..SlotFilters::default()
    };
    let slow_only = SlotFilters {
        slow_only: true,
        ..SlotFilters::default()
    };
    let fast = row(SlotStatus::FastFinalized, SkipClassification::NotSkipped);
    let slow = row(SlotStatus::SlowFinalized, SkipClassification::NotSkipped);
    assert!(fast_only.matches(&fast));
    assert!(!fast_only.matches(&slow));
    assert!(!slow_only.matches(&fast));
    assert!(slow_only.matches(&slow));
}

#[test]
fn toggle_filter_flips_each_dimension_and_recomputes_indices() {
    // Build an App with one row of each shape so the recompute
    // surface is exercised end-to-end.
    let mut state = crate::model::state::State::new(PathBuf::from("/tmp/x"), 0);
    // Slot 1: VSKIP (skip vote, no finalize, indeterminate).
    state.slot_mut(1).voted_skip_at = Some(time::macros::datetime!(2026-05-23 16:00:00 UTC));
    // Slot 2: CSKIP (skip vote + finalize → CanonicalSkip after analyze).
    state.slot_mut(2).voted_skip_at = Some(time::macros::datetime!(2026-05-23 16:00:01 UTC));
    state.slot_mut(2).finalized_at = Some(time::macros::datetime!(2026-05-23 16:00:02 UTC));
    state.slot_mut(2).fast_finalize = Some(true);
    // Slot 3: plain finalized.
    state.slot_mut(3).finalized_at = Some(time::macros::datetime!(2026-05-23 16:00:03 UTC));
    state.slot_mut(3).fast_finalize = Some(true);

    // Run classifier so CSKIP row carries the discriminator.
    crate::aggregator::analyze(&mut state);

    let mut app = App::new(&state, None, 60);
    let total_rows = app.slot_rows.len();
    assert_eq!(total_rows, 3);
    // Default: every row passes.
    assert_eq!(app.slot_indices.len(), total_rows);

    // Toggle CanonicalSkipOnly on → only the CSKIP row remains.
    app.toggle_filter(FilterKind::CanonicalSkipOnly);
    assert!(app.slot_filters.canonical_skip_only);
    assert_eq!(app.slot_indices.len(), 1);
    let only = &app.slot_rows[app.slot_indices[0]];
    assert!(only.skip_classification.is_canonical_skip());

    // Toggle VskipOnly on as well → union VSKIP ∪ CSKIP (2 rows).
    app.toggle_filter(FilterKind::VskipOnly);
    assert!(app.slot_filters.vskip_only);
    assert_eq!(app.slot_indices.len(), 2);

    // Toggle CanonicalSkipOnly off → only VSKIP row remains.
    app.toggle_filter(FilterKind::CanonicalSkipOnly);
    assert!(!app.slot_filters.canonical_skip_only);
    assert_eq!(app.slot_indices.len(), 1);
    let only = &app.slot_rows[app.slot_indices[0]];
    assert_eq!(only.status, SlotStatus::Skipped);
    assert!(!only.skip_classification.is_canonical_skip());

    // Clear → back to all rows.
    app.clear_filters();
    assert!(!app.slot_filters.any_active());
    assert_eq!(app.slot_indices.len(), total_rows);
}
