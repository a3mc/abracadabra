//! Empirical check: run the parser against the captured log and
//! confirm we recognise the same shred-pipeline datapoints `grep`
//! counts. Skipped silently when the captured log is absent so CI
//! does not require checked-in fixtures.

#![allow(clippy::expect_used)] // test-only path; failure is loud by design.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use abracadabra::parser::{parse, EventKind, MetricEvent, Parsed};

fn captured_log() -> Option<PathBuf> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)?;
    let p = repo_root.join("log").join("new-log-to-debug.log");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[derive(Debug, Default)]
struct Counts {
    shred_fetch: u64,
    shred_fetch_repair: u64,
    shred_sigverify: u64,
    recv_window_insert: u64,
    blockstore_insert: u64,
    shred_recovery: u64,
    shred_insert_is_full: u64,
    retransmit_first_shred: u64,
    retransmit_slot_stats: u64,
    slot_tracking: u64,
    leader_slot_elapsed: u64,
    broadcast_shreds: u64,
    banking_scheduler: u64,
    slot_metrics: u64,
}

fn ingest(path: &PathBuf) -> Counts {
    let f = File::open(path).expect("open captured log");
    let mut counts = Counts::default();
    for line in BufReader::with_capacity(64 * 1024, f).lines() {
        let line = line.expect("read line");
        let Ok(Parsed::Event(ev)) = parse(&line) else {
            continue;
        };
        if let EventKind::Metric(m) = ev.kind {
            match m {
                MetricEvent::ShredFetch { .. } => counts.shred_fetch += 1,
                MetricEvent::ShredFetchRepair { .. } => counts.shred_fetch_repair += 1,
                MetricEvent::ShredSigverify { .. } => counts.shred_sigverify += 1,
                MetricEvent::RecvWindowInsert { .. } => counts.recv_window_insert += 1,
                MetricEvent::BlockstoreInsert { .. } => counts.blockstore_insert += 1,
                MetricEvent::ShredRecovery { .. } => counts.shred_recovery += 1,
                MetricEvent::ShredInsertIsFull { .. } => counts.shred_insert_is_full += 1,
                MetricEvent::RetransmitFirstShred { .. } => counts.retransmit_first_shred += 1,
                MetricEvent::RetransmitSlotStats { .. } => counts.retransmit_slot_stats += 1,
                MetricEvent::SlotTracking { .. } => counts.slot_tracking += 1,
                MetricEvent::LeaderSlotElapsed { .. } => counts.leader_slot_elapsed += 1,
                MetricEvent::BroadcastShreds { .. } => counts.broadcast_shreds += 1,
                MetricEvent::BankingStageCounts { .. } => counts.banking_scheduler += 1,
                MetricEvent::SlotMetrics { .. } => counts.slot_metrics += 1,
            }
        }
    }
    counts
}

#[test]
fn parser_recognises_expected_metric_counts() {
    let Some(path) = captured_log() else {
        eprintln!("[skip] log/new-log-to-debug.log not present; skipping empirical check");
        return;
    };
    let c = ingest(&path);

    // Expected counts come from `grep -c "datapoint: <name> " <log>`
    // — see scripts/check-metric-counts.sh. Discrepancies usually mean
    // the parser's name dispatch is wrong, the datapoint format
    // shifted upstream, or a required field disappeared. The numbers
    // below are pinned to the captured log; update them and the
    // script together if the log fixture is regenerated.
    assert_eq!(c.shred_fetch, 5461, "shred_fetch");
    assert_eq!(c.shred_fetch_repair, 4632, "shred_fetch_repair");
    assert_eq!(c.shred_sigverify, 7963, "shred_sigverify");
    assert_eq!(c.recv_window_insert, 8037, "recv-window-insert-shreds");
    assert_eq!(c.blockstore_insert, 8037, "blockstore-insert-shreds");
    assert_eq!(c.shred_recovery, 8037, "shred-recovery");
    assert_eq!(c.shred_insert_is_full, 13_759, "shred_insert_is_full");
    assert_eq!(c.retransmit_first_shred, 13_623, "retransmit-first-shred");
    assert_eq!(
        c.retransmit_slot_stats, 13_623,
        "retransmit-stage-slot-stats"
    );
    assert_eq!(c.slot_tracking, 14_827, "event_handler_slot_tracking");
}
