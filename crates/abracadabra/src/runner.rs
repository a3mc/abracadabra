//! End-to-end pipeline: open file → stream lines → parse → ingest → summarize.
//!
//! Owned by `main.rs` so the binary stays thin and the runner is testable.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

use thiserror::Error;

use crate::aggregator;
use crate::model::alerts::Severity;
use crate::model::analysis;
use crate::model::state::State;
use crate::parser::{self, Parsed};

#[derive(Debug, Error)]
pub enum RunError {
    #[error("failed to stat log file {path:?}: {source}")]
    Stat {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open log file {path:?}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read error after {lines} lines: {source}")]
    Read {
        lines: u64,
        #[source]
        source: std::io::Error,
    },
}

/// Per-run counters not stored on State (transient).
#[derive(Debug, Default)]
pub struct RunStats {
    pub continuation_lines: u64,
    pub ignored_lines: u64,
    pub parse_errors: u64,
    pub elapsed_ms: u128,
}

/// Stream the log at `path` through parser → aggregator. Returns the
/// finalized State plus per-run counters.
pub fn run(path: PathBuf) -> Result<(State, RunStats), RunError> {
    let metadata = std::fs::metadata(&path).map_err(|source| RunError::Stat {
        path: path.clone(),
        source,
    })?;
    let size_bytes = metadata.len();

    let file = File::open(&path).map_err(|source| RunError::Open {
        path: path.clone(),
        source,
    })?;
    let reader = BufReader::with_capacity(64 * 1024, file);

    let mut state = State::new(path, size_bytes);
    let mut stats = RunStats::default();
    let started = Instant::now();

    for line in reader.lines() {
        let line = line.map_err(|source| RunError::Read {
            lines: state.file_meta.line_count,
            source,
        })?;
        state.file_meta.line_count = state.file_meta.line_count.saturating_add(1);

        match parser::parse(&line) {
            Ok(Parsed::Event(ev)) => aggregator::ingest(&mut state, ev),
            Ok(Parsed::Issue {
                ts,
                level,
                module,
                body,
            }) => aggregator::ingest_issue(&mut state, ts, level, module, body),
            Ok(Parsed::Continuation) => {
                stats.continuation_lines = stats.continuation_lines.saturating_add(1);
            }
            Ok(Parsed::Ignored) => {
                stats.ignored_lines = stats.ignored_lines.saturating_add(1);
            }
            Err(_) => {
                stats.parse_errors = stats.parse_errors.saturating_add(1);
            }
        }
    }

    aggregator::analyze(&mut state);
    stats.elapsed_ms = started.elapsed().as_millis();

    Ok((state, stats))
}

/// Print a human-readable summary to stdout.
///
/// Each section is curated for signal: deltas between related counts are
/// computed inline, ratios named, and verdicts marked with `[✓]`. Pure
/// number-dumping is avoided.
pub fn print_summary(state: &State, stats: &RunStats) {
    let meta = &state.file_meta;
    let ov = &state.overall;

    let rate = if stats.elapsed_ms > 0 {
        meta.line_count * 1000 / u64::try_from(stats.elapsed_ms).unwrap_or(u64::MAX)
    } else {
        0
    };

    // -- Header
    println!("\n== abracadabra ==");
    println!("{}", meta.path.display());
    println!(
        "  {:.2} GB | {} lines | {} ms parse | {} lines/sec",
        meta.size_bytes as f64 / 1_073_741_824.0,
        commas(meta.line_count),
        stats.elapsed_ms,
        commas(rate),
    );
    if let Some((lo, hi)) = meta.time_range {
        let dur = hi - lo;
        println!("  {lo} -> {hi}");
        println!(
            "  duration: {}h {}m {}s",
            dur.whole_hours(),
            dur.whole_minutes() % 60,
            dur.whole_seconds() % 60,
        );
    }
    if let Some((lo, hi)) = slot_range(state) {
        println!(
            "  slots {} -> {}  ({} distinct)",
            commas(lo),
            commas(hi),
            commas(state.slots.len() as u64),
        );
    }
    println!(
        "  ingested: {} events | {} ignored | {} continuations | {} parse-err",
        commas(
            meta.line_count
                .saturating_sub(stats.ignored_lines)
                .saturating_sub(stats.continuation_lines)
                .saturating_sub(stats.parse_errors)
        ),
        commas(stats.ignored_lines),
        commas(stats.continuation_lines),
        commas(stats.parse_errors),
    );

    // -- Health snapshot
    println!("\n-- Health --");
    let total_final = ov.finalized_fast.saturating_add(ov.finalized_slow);
    let fast_pct = pct(ov.finalized_fast, total_final);
    health_line(
        "fast-finalize",
        &format!("{fast_pct:>5.2}%"),
        fast_pct >= 80.0,
        if fast_pct >= 80.0 {
            "healthy (>=80%)"
        } else {
            "DEGRADED — cluster fragmented"
        },
    );
    let total_slots = state.slots.len() as u64;
    // Bad-skip rate is the operator-facing failure indicator. Numerator
    // is skips we proved landed on canonical slots (Stage 1 classifier);
    // denominator is total skips we cast. When indeterminate skips exist
    // the displayed number is a lower bound — marked with `>=`.
    let bad_skips = ov.bad_skips_direct.saturating_add(ov.bad_skips_ancestry);
    let bad_skip_pct = pct(bad_skips, ov.votes_skip);
    let bound_marker = if ov.indeterminate_skips > 0 { ">=" } else { "  " };
    println!(
        "  {:<18} {}{:>5.2}%   {} bad of {} skips{}",
        "bad-skip rate",
        bound_marker,
        bad_skip_pct,
        commas(bad_skips),
        commas(ov.votes_skip),
        if ov.indeterminate_skips > 0 {
            format!(" ({} indeterminate)", commas(ov.indeterminate_skips))
        } else {
            String::new()
        },
    );
    let skip_pct = pct(ov.votes_skip, total_slots);
    println!(
        "  {:<18} {:>6}   {} of {} slots (raw — see bad-skip rate above)",
        "vote skip rate",
        format!("{skip_pct:.2}%"),
        commas(ov.votes_skip),
        commas(total_slots),
    );
    health_line(
        "standstills",
        &commas(ov.standstill_events),
        ov.standstill_events == 0,
        "no liveness issues",
    );
    let frag = ov.safe_to_notar.saturating_add(ov.safe_to_skip);
    let observed_hours = meta
        .time_range
        .map_or(1.0, |(lo, hi)| (hi - lo).as_seconds_f64() / 3600.0)
        .max(1.0);
    println!(
        "  {:<18} {:>6}   SafeToNotar+Skip ({:.2}/h)",
        "fragmentation",
        commas(frag),
        frag as f64 / observed_hours,
    );
    println!(
        "  {:<18} {:>6}   leader windows that timed out",
        "crashed leaders",
        commas(ov.timeout_crashed_leaders),
    );
    health_line(
        "refresh votes",
        &commas(ov.refreshing_votes),
        ov.refreshing_votes == 0,
        "no standstill recoveries",
    );

    // -- Leadership
    if ov.produce_windows > 0 {
        let leader_slots = state.slots.values().filter(|s| s.we_are_leader).count() as u64;
        let stake_share = pct(leader_slots, total_slots);
        println!("\n-- Leadership --");
        println!(
            "  our leader windows {:>6}   {} slots | ~{:.2}% stake share",
            commas(ov.produce_windows),
            commas(leader_slots),
            stake_share,
        );
    }

    // -- Per-slot lifecycle latency
    print_lifecycle_latency_section(state);

    // -- Vote-resume time after TimeoutCrashedLeader
    print_resume_section(state);

    // -- Voting gaps (consecutive slots with no local vote)
    print_voting_gaps_section(state);

    // -- Vote / cert flow with computed deltas
    println!("\n-- Vote / cert flow --");
    let notar_to_final_drop = ov.votes_notarize.saturating_sub(ov.votes_finalize);
    println!(
        "  {:<22} {:>10}",
        "We voted notarize",
        commas(ov.votes_notarize)
    );
    if notar_to_final_drop > 0 {
        println!(
            "       |  -{}  (cast fallback / bad_window, no Finalize)",
            commas(notar_to_final_drop),
        );
    }
    println!(
        "  {:<22} {:>10}",
        "We voted finalize",
        commas(ov.votes_finalize)
    );
    println!();
    let true_fb = ov
        .block_notar_fallback_count
        .saturating_sub(ov.block_notarized_count);
    let true_fb_pct = pct(true_fb, ov.block_notar_fallback_count);
    println!(
        "  {:<22} {:>10}   cluster",
        "Block Notarized",
        commas(ov.block_notarized_count),
    );
    println!(
        "  {:<22} {:>10}   +{} TRUE fallbacks ({:.3}% — {})",
        "Block notar-fallback",
        commas(ov.block_notar_fallback_count),
        commas(true_fb),
        true_fb_pct,
        if true_fb_pct < 0.5 {
            "rare/healthy"
        } else {
            "elevated"
        },
    );
    println!(
        "  {:<22} {:>10}   fast: {} ({:.2}%) | slow: {}",
        "Finalized",
        commas(total_final),
        commas(ov.finalized_fast),
        fast_pct,
        commas(ov.finalized_slow),
    );

    // -- Block lifecycle flow
    println!("\n-- Block lifecycle flow --");
    let shred_to_freeze = ov.bank_frozen_count.saturating_sub(ov.first_shreds);
    let freeze_to_root = ov.bank_frozen_count.saturating_sub(ov.setting_root_count);
    let setting_to_new = ov.setting_root_count.saturating_sub(ov.new_root_count);
    println!("  {:<22} {:>10}", "First shred", commas(ov.first_shreds));
    if shred_to_freeze > 0 {
        println!(
            "       |  +{}  (banks frozen without first-shred event, repair path)",
            commas(shred_to_freeze),
        );
    }
    println!(
        "  {:<22} {:>10}",
        "Bank frozen",
        commas(ov.bank_frozen_count)
    );
    if freeze_to_root > 0 {
        println!(
            "       |  -{}  (frozen but not rooted — forks pruned)",
            commas(freeze_to_root),
        );
    }
    println!(
        "  {:<22} {:>10}",
        "setting root",
        commas(ov.setting_root_count)
    );
    if setting_to_new > 0 {
        println!(
            "       |  -{}  (bank-forks pruning in-flight at log boundary)",
            commas(setting_to_new),
        );
    }
    println!("  {:<22} {:>10}", "new root", commas(ov.new_root_count));

    // -- Cluster-slots loose-end signals (only if any signal present)
    if ov.no_epoch_metadata > 0 || ov.cluster_slots_service_stopped {
        let rate_per_sec = ov.no_epoch_metadata as f64 / (observed_hours * 3600.0);
        println!("\n-- Cluster-slots loose end --");
        println!(
            "  No epoch_metadata     {:>10}   ~{:.2}/sec sustained",
            commas(ov.no_epoch_metadata),
            rate_per_sec,
        );
        println!(
            "  Service stopped line  {:>10}   (in this log)",
            if ov.cluster_slots_service_stopped {
                "yes"
            } else {
                "no"
            }
        );
    }

    // -- Alerts
    if state.alerts.is_empty() {
        println!("\n-- Alerts --   (none)");
    } else {
        println!("\n-- Alerts ({}) --", state.alerts.len());
        for a in &state.alerts {
            let tag = match a.severity {
                Severity::Info => "[INFO]",
                Severity::Warn => "[WARN]",
                Severity::Critical => "[CRIT]",
            };
            println!("  {tag} {}", a.description);
        }
    }
    println!();
}

fn health_line(label: &str, value: &str, healthy: bool, note: &str) {
    let mark = if healthy { "[✓]" } else { "[✗]" };
    println!("  {label:<18} {value:>6}   {mark} {note}");
}

fn pct(num: u64, denom: u64) -> f64 {
    if denom == 0 {
        0.0
    } else {
        num as f64 * 100.0 / denom as f64
    }
}

#[allow(clippy::cast_possible_truncation)]
fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn slot_range(state: &State) -> Option<(u64, u64)> {
    let min = state.slots.keys().next()?;
    let max = state.slots.keys().next_back()?;
    Some((*min, *max))
}

fn print_lifecycle_latency_section(state: &State) {
    let latencies = analysis::lifecycle_latencies(state);
    if latencies.is_empty() {
        return;
    }
    let mut us_values: Vec<i64> = latencies.iter().map(|r| r.us).collect();
    us_values.sort_unstable();
    let p50 = analysis::percentile(&us_values, 0.50).unwrap_or(0);
    let p95 = analysis::percentile(&us_values, 0.95).unwrap_or(0);
    let p99 = analysis::percentile(&us_values, 0.99).unwrap_or(0);
    let max = us_values.last().copied().unwrap_or(0);

    println!("\n-- Slot lifecycle latency (first shred -> finalized) --");
    println!(
        "  finalized slots with full timeline: {}",
        commas(latencies.len() as u64),
    );
    println!("  p50:  {:>8.1} ms", p50 as f64 / 1000.0);
    println!("  p95:  {:>8.1} ms", p95 as f64 / 1000.0);
    println!("  p99:  {:>8.1} ms", p99 as f64 / 1000.0);
    println!("  max:  {:>8.1} ms", max as f64 / 1000.0);

    let mut sorted = latencies;
    sorted.sort_by_key(|r| std::cmp::Reverse(r.us));
    println!("\n  Top 5 slowest slots:");
    for r in sorted.iter().take(5) {
        let tag = match r.fast {
            Some(true) => "fast",
            Some(false) => "slow",
            None => "?",
        };
        println!(
            "    slot {:>11}   {:>8.1} ms   ({tag})",
            commas(r.slot),
            r.us as f64 / 1000.0,
        );
    }
}

fn print_resume_section(state: &State) {
    let recs = analysis::vote_resumes_after_tcl(state);
    if recs.is_empty() {
        return;
    }
    let mut us_values: Vec<i64> = recs.iter().map(|r| r.resume_us).collect();
    us_values.sort_unstable();
    let p50 = analysis::percentile(&us_values, 0.50).unwrap_or(0);
    let p95 = analysis::percentile(&us_values, 0.95).unwrap_or(0);
    let p99 = analysis::percentile(&us_values, 0.99).unwrap_or(0);
    let max = us_values.last().copied().unwrap_or(0);

    println!(
        "\n-- Leader timeouts / vote-resume (TimeoutCrashedLeader -> next Voting notarize) --"
    );
    println!("  TCL events:   {}", commas(recs.len() as u64));
    println!("  resume p50:  {:>8.2} s", p50 as f64 / 1_000_000.0);
    println!("  resume p95:  {:>8.2} s", p95 as f64 / 1_000_000.0);
    println!("  resume p99:  {:>8.2} s", p99 as f64 / 1_000_000.0);
    println!("  resume max:  {:>8.2} s", max as f64 / 1_000_000.0);

    let mut sorted = recs;
    sorted.sort_by_key(|r| std::cmp::Reverse(r.resume_us));
    println!("\n  Top 5 longest vote-resume times:");
    for r in sorted.iter().take(5) {
        let slot_gap = r.resume_slot.saturating_sub(r.tcl_slot);
        println!(
            "    TCL {:>11} -> notarized {:>11}   {:>6.2} s  (+{} slots)",
            commas(r.tcl_slot),
            commas(r.resume_slot),
            r.resume_us as f64 / 1_000_000.0,
            slot_gap,
        );
    }
}

fn print_voting_gaps_section(state: &State) {
    // Threshold: >=2 consecutive unvoted slots (single gaps happen at log start/end edges).
    let gaps = analysis::voting_gaps(state, 2);
    println!("\n-- Voting gaps (>=2 consecutive slots with no Notarize/Skip vote) --");
    if gaps.is_empty() {
        println!("  none — local node voted on every slot in range  [✓]");
        return;
    }
    println!("  count: {}", commas(gaps.len() as u64));
    let mut sorted = gaps;
    sorted.sort_by_key(|g| std::cmp::Reverse(g.gap_slots));
    println!("\n  Top 5 largest gaps:");
    for g in sorted.iter().take(5) {
        let duration = (g.resume_vote_at - g.last_vote_at).whole_seconds();
        println!(
            "    slots {} -> {}  ({} slots, {} s wall clock)",
            commas(g.start_slot),
            commas(g.end_slot),
            commas(g.gap_slots),
            duration,
        );
    }
}
