# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2] — 2026-05-28

Version aligned with the alpenglow `ag-v0.3.2` tag this release was
validated against.

### Added

- Stage 1 canonical-skip classifier (`aggregator::classify_skips`):
  walks parent pointers from observed `Finalized` slots, assigns
  every `voted_skip` slot to `DirectFinalize` / `Ancestry` /
  `Indeterminate`. The headline operator-facing metric
  `canonical-skip %` is derived from this classification.
- `EventKind::TriggeringParentReady` parser variant for the
  `event_handler::add_missing_parent_ready` log line; counter on
  `OverallStats` (intentionally unsurfaced, reserved for future
  per-window analysis).
- Standstill range tracking: `OverallStats.standstill_ranges` built
  from `StandstillExtending` / `StandstillEnded` events, including
  EOS-orphan close. New `timeout_crashed_leaders_outside_standstill`
  counter computed in `analyze` filters TCLs that fired inside a
  standstill window (where the per-slot timeout is stretched and the
  leader did not actually misbehave).
- `SlotViewRow.consensus_inverted` flag rendered as `↶` in the
  Slots-tab `consensus` column for rows where the cluster's
  `Finalized` cert arrived before the local node's `Block` event
  (cluster outran local replay). Plain `-` is now reserved for
  genuinely missing data.
- `theme::TRUE_FB_ELEVATED_PCT` threshold constant.
- `ProduceWindow` corruption guards (oversized span `> 16`,
  inverted range `end < start`), counted in
  `OverallStats.malformed_produce_window`.
- `MirrorSparkline` widget on the time-series tab for the paired
  vote-skip / canonical-skip mirror chart (one rises from the
  bottom, the other hangs from the top, shared x-axis).

### Changed

- Slot status labels: `SKIP` renamed to `VSKIP` (we Voted skip,
  no canonical evidence). `CSKIP` unchanged. The `V` / `C` prefix
  convention makes both labels self-describing — both refer to
  *our* vote; the prefix indicates whether canonical evidence
  exists.
- Slots-tab filter hotkeys rotated:
  `[v]` = VSKIP only, `[c]` = CSKIP only,
  `[s]` = SLOW, `[x]` = clear all filters,
  `[b]` freed. `SlotFilters::vskip_only` replaces `skipped_only`
  with narrowed semantic; the skip-family pair (`vskip_only`,
  `canonical_skip_only`) now uses OR semantics — pressing both
  shows the union.
- Slots-tab `consensus` column now distinguishes three states:
  positive value in ms (normal), `↶` (cluster outran local
  replay), `-` (missing data).
- Slots-tab `Latency bands` reference panel adds a `consensus`
  threshold row (≤ 300 ms · 300–600 ms · > 600 ms).
- Slots-tab KPI strip: leader-slot share metric moved from the
  buried right-panel footer onto `slot stats` line 2 next to the
  lifecycle percentiles. `p95` value is now health-coloured at the
  KPI site, replacing the previous footer row.
- Slots-tab legend: static column-value reference (status, vote,
  consensus glyph) moved to the bottom of the panel below the
  `[x] clear all filters` separator. The filter section above is
  now exclusively interactive toggles.
- Slots-panel title shortened: drops the redundant total in
  `slots (N total | cursor M / N)` → `slots (cursor M / N)`. The
  filtered variant retains `M of N`.
- `vote & cert totals` widget on Overview rewritten as three
  horizontal columns (`votes` / `certs` / `finalized`) instead
  of stacked text. Headings styled in `title_style`.
- Overview headline-health `fast-finalize` row dropped the
  `fast NN.NN% / slow NN.NN%` breakdown (the numbers now live
  in the `vote & cert totals` `finalized` column). Verdict mark
  and text retained.
- Alerts-tab `detail` block + list block + Overview `alerts`
  widget gain `Padding::new(...)` for breathing room from the
  block border.
- Alerts-tab `detail` widget adds explicit blank lines between
  the `last` and `span` rows and between the `first sample`
  label and the sample body.
- Leader-timeouts trend chart uses a packing-search algorithm
  to pick `(bar_width, bar_count)` that fills the panel width.
  `bar_width` capped at 6 columns so very wide terminals do not
  collapse the chart to a handful of thick bars.
- Header `crashed leaders` metric on Overview adopts the
  standstill-aware count when standstill ranges exist (raw count
  appears on a continuation line); when no standstill ranges
  exist, the single-line display is unchanged.
- Canonical-skip lower-bound marker (`≥`) now applied on the
  Slots-tab KPI strip when `indeterminate_skips > 0`, matching
  the header, overview, windows, and `--text` runner surfaces.
- `stake share` label removed. Renamed `leader-slot share`
  (window-relative, not on-chain stake — the previous label
  overstated what the tool measures).
- `Severity::from_us` thresholds (1.5 s / 3.0 s) documented as
  provisional pending empirical calibration across multi-day
  log archives.
- True-FB cutoff `0.5%` consolidated into
  `theme::TRUE_FB_ELEVATED_PCT` and consumed at both render
  sites (was a duplicated literal).
- True-FB percentage formatted as `{:.2}%` (was `{:.3}%` — no
  false precision on an integer-count ratio).

### Fixed

- Slots-tab canonical-skip percentage was missing the `≥`
  lower-bound marker even when indeterminate skips existed.
  Same number now reads consistently across all surfaces.
- Seven previously-undocumented `OverallStats` fields received
  reserved-for-future docstrings explicitly marking them as
  intentionally unsurfaced (matching the existing
  `parent_ready_recoveries` precedent): `malformed_produce_window`,
  `standstill_extending_events`, `standstill_ended_events`,
  `no_epoch_info_for_slot`, `updating_epoch_metadata`,
  `evicting_epoch_metadata`, `invalid_cluster_slots_update`.

### Internal

- Test count: 187 → 201 across the workspace.
- `aggregator/tests.rs` (989 LOC) split into 6 sub-modules under
  `aggregator/tests/` (`ingest`, `standstill`, `classify_skips`,
  `log_patterns`, `produce_window`, `analyze_alerts`).
- `tui/app.rs` inline `mod tests` (309 LOC) extracted to
  `tui/app_tests.rs` via `#[path]` declaration; production
  module drops to 753 LOC, under the 800 strong-warn threshold.
- `OverallStats` unique-slot count docstrings clarified —
  `finalized_slot_count + skipped_slot_count + pending_slot_count`
  is overlap, not partition (a canonical-skip slot is counted in
  both `finalized_slot_count` and `skipped_slot_count`).
- rustfmt + strict clippy fixes for the workspace:
  `explicit_iter_loop`, `doc_overindented_list_items`,
  `type_complexity`, `items_after_statements`, `derivable_impls`.

## [0.1.1] — 2026-05-27

### Added

- Slots tab: new `m` filter for rows where the validator voted
  both Notarize AND Skip on the same slot. Mixed-vote rows now
  display as `N+S` / `N+F+S` (previously the Skip was silently
  dropped from the vote-pattern column).
- Memory ceilings on aggregator state: `LogIssueGroup.timestamps`
  capped at 1,000,000 entries with overflow counter;
  `ProduceWindow` events rejected when `end - start > 16` (4×
  the Alpenglow `NUM_CONSECUTIVE_LEADER_SLOTS = 4` invariant)
  with `malformed_produce_window` counter. Defence against
  memory-exhaustion on pathological / corrupted log lines.

### Changed

- Yank target moved from `/tmp/abracadabra-yank-N.txt` to
  `$XDG_RUNTIME_DIR/abracadabra/abracadabra-yank-<pid>-<n>.txt`
  (fallback `$HOME/.cache/abracadabra/yank/...`). Per-process
  pid in the filename prevents cross-session collisions in the
  persistent fallback dir.
- Windows / Overview / Slots / Leader-timeouts tabs share a
  single precomputed `LatencySnapshot`; eliminates per-frame
  O(n log n) recomputation on the render path.
- Alerts list viewport follows the `j/k` cursor (`ListState`).

### Fixed

- Alerts list orders globally by `(severity desc, at asc)`.
  The TUI title's "CRIT first, by count" promise is now
  load-bearing across inline + LogPattern alerts.
- `SlotRecord::delta_us` returns `None` for inverted intervals
  (was returning negative microseconds and polluting percentile
  calculations downstream).
- `Severity::from_us` no longer mis-classifies negative input
  as Normal via integer-division truncation.
- Window slot-duration percentiles count only strictly adjacent
  `(n, n+1)` pairs; gap-separated pairs no longer inflate p95.
- `percentile()` clamps `p` to `[0.0, 1.0]` (NaN → 0.0).
- `fast_slow_pct()` partitions exactly to 100.
- Per-hour rate displays use actual log duration (was clamped
  to a 1-hour minimum, deflating sub-hour rates up to ~60×).
- Scroll keys on non-list tabs no longer clobber the Slots
  cursor position.
- Single-metric sparkline cards fill the full panel width.
- StackedBars paints `Color::Reset` on every cell branch.
- Stale `/tmp` references and `tui/mod.rs` module doc updated.

### Security

- Panic hook restores terminal state before propagating;
  mid-render panic no longer leaves a wedged terminal.
- Yank file write uses `O_CREAT | O_EXCL | O_NOFOLLOW`;
  symlink attacks on the previous `/tmp` target neutralised.
- TUI sanitises log-derived spans before rendering (strips
  ESC, DEL, replaces other C0). Crafted log lines cannot
  cursor-manipulate the terminal.
- Parser sanitises `Parsed::Issue` bodies at construction:
  strips ESC/DEL/LF/CR, replaces other C0 with `?`, preserves
  multi-byte UTF-8. CSI sequences cannot reach stdout via
  `--text` mode.
- Hash captures length-bounded `{32,48}` on every parse path
  (regex and `strip_prefix`). Slot-digit captures bounded `{1,20}`.
- `ProduceWindow { start, end }` clamped at `end - start ≤ 16`;
  malformed log lines with `end = u64::MAX` cannot trigger
  exabyte-scale allocation.
- Removed unused `AlertKind::LeaderTimeoutCrashed` variant.
- `cargo audit`: 0 vulnerabilities, 0 warnings (95 transitive
  deps, advisory DB 2026-05-23).
- Still 0 `unsafe` blocks in project sources.

### Internal

- Test count 84 → 187 across the workspace.
- `aggregator/mod.rs` test module extracted to sibling
  `aggregator/tests.rs` (mod.rs 845 → 484 LOC).

## [0.1.0] — 2026-05-26

### Added

- Initial public release.
- Streaming line parser for Solana Alpenglow validator logs
  (tested against `ag-v0.3.2`; Alpenglow is in active development
  upstream and log formats may shift — file an issue if parsing
  breaks on a newer cluster version). Recognises 16 votor events,
  root-utils events,
  bank events, `solana_core::cluster_slots_service` known-issue
  lines, and `solana_metrics::metrics` datapoints.
- Per-`(severity, module)` aggregation of unparsed WARN/ERROR log
  lines into `LogPattern` alerts with timestamp tracking.
- TUI dashboard with six tabs:
  - **Overview** — file metadata, headline health verdicts (3-tier
    bands for fast-finalize %, FIN %, vote skip rate), vote/cert
    totals, latency-stage breakdown, leader-timeout summary.
  - **Time series** — 10-card grid bucketed by configurable
    interval. Stacked-bar fast-vs-slow finalize chart with sub-cell
    block-character precision; single-series sparklines (baseline-
    subtracted) for the remaining 9 metrics.
  - **Windows** — rolling-window comparison across 24h / 12h / 6h /
    3h / 1h windows.
  - **Slots** — KPI strip + scrollable filterable per-slot table.
    Seven keyboard-toggleable filter dimensions covering status,
    finalization path, leader role, and event tags.
  - **Leader timeouts** — `TimeoutCrashedLeader` analysis with
    severity bands, distribution histogram, per-bucket trend bars,
    incident list.
  - **Alerts** — severity rollup, grouped alert list with per-
    pattern timestamp sparklines, copy-to-file via `y` key.
- CLI flags `--text` (non-interactive summary), `--bucket <DUR>`
  (time-series bucket size, 1m..=24h), `--version`.
- GitHub Actions CI (`cargo fmt --check`, build, test, strict
  `clippy --all-targets`, `xtask lint-prod`) on `ubuntu-22.04`.
- Tag-triggered binary release workflow building
  `x86_64-unknown-linux-gnu` (glibc 2.35+ compatible) and
  `x86_64-unknown-linux-musl` (static, distro-agnostic), each with
  SHA-256 checksum and auto-generated release notes.
- Dual-licensed under MIT OR Apache-2.0.

### Security

- `cargo audit`: 0 vulnerabilities, 0 warnings against 95
  transitive dependencies (advisory DB 2026-05-23).
- 0 `unsafe` blocks in project sources.

[Unreleased]: https://github.com/matsuro-hadouken/abracadabra/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/matsuro-hadouken/abracadabra/releases/tag/v0.1.1
[0.1.0]: https://github.com/matsuro-hadouken/abracadabra/releases/tag/v0.1.0
