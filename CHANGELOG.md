# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/matsuro-hadouken/abracadabra/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/matsuro-hadouken/abracadabra/releases/tag/v0.1.0
