# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
