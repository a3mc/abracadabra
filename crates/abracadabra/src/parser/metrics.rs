//! `solana_metrics::metrics` datapoint extraction (shred pipeline).
//!
//! Recognises the 10 shred-related datapoints the Live tab and the
//! Time Series tab consume. Format of every datapoint line:
//!
//! ```text
//! datapoint: <name> key1=value1 key2=value2 ...
//! ```
//!
//! Values use the InfluxDB line-protocol type suffix convention:
//! integers carry a trailing `i` (e.g. `771i`), booleans render as
//! `true` / `false`, floats are bare. We parse only the fields we
//! plan to surface; everything else on the line is intentionally
//! dropped to keep memory bounded.

use crate::parser::{EventKind, MetricEvent};

/// Extract a recognised shred-pipeline datapoint from `body`, or
/// `None` if the line is not a shred-pipeline datapoint.
///
/// Unknown datapoint names return `None`. Recognised names with
/// missing required fields also return `None` (treated as malformed
/// rather than silently emitting an event with zeroed fields).
pub fn parse_body(body: &str) -> Option<EventKind> {
    let rest = body.strip_prefix("datapoint: ")?;
    let (name, fields) = rest.split_once(' ')?;
    // A handful of datapoints embed comma-separated tags in the name
    // (e.g. `retransmit-stage,is_xdp=false`); strip those by taking
    // only the substring before the first comma. None of the 10
    // datapoints we currently parse use the comma form, but the
    // guard prevents future surprises.
    let name = name.split(',').next()?;
    let metric = match name {
        "shred_fetch" => parse_shred_fetch(fields)?,
        "shred_fetch_repair" => parse_shred_fetch_repair(fields)?,
        "shred_sigverify" => parse_shred_sigverify(fields)?,
        "recv-window-insert-shreds" => parse_recv_window_insert(fields)?,
        "blockstore-insert-shreds" => parse_blockstore_insert(fields)?,
        "shred-recovery" => parse_shred_recovery(fields)?,
        "shred_insert_is_full" => parse_shred_insert_is_full(fields)?,
        "retransmit-first-shred" => parse_retransmit_first_shred(fields)?,
        "retransmit-stage-slot-stats" => parse_retransmit_slot_stats(fields)?,
        "event_handler_slot_tracking" => parse_slot_tracking(fields)?,
        _ => return None,
    };
    Some(EventKind::Metric(metric))
}

// ---- per-datapoint parsers --------------------------------------------------

fn parse_shred_fetch(fields: &str) -> Option<MetricEvent> {
    let shred_count = field_u64(fields, "shred_count")?;
    Some(MetricEvent::ShredFetch { shred_count })
}

fn parse_shred_fetch_repair(fields: &str) -> Option<MetricEvent> {
    let shred_count = field_u64(fields, "shred_count")?;
    Some(MetricEvent::ShredFetchRepair { shred_count })
}

fn parse_shred_sigverify(fields: &str) -> Option<MetricEvent> {
    Some(MetricEvent::ShredSigverify {
        num_packets: field_u64(fields, "num_packets")?,
        num_discards: field_u64(fields, "num_discards_pre")
            .unwrap_or(0)
            .saturating_add(field_u64(fields, "num_discards_post").unwrap_or(0)),
        num_duplicates: field_u64(fields, "num_duplicates").unwrap_or(0),
        elapsed_micros: field_u64(fields, "elapsed_micros")?,
    })
}

fn parse_recv_window_insert(fields: &str) -> Option<MetricEvent> {
    Some(MetricEvent::RecvWindowInsert {
        num_shreds_received: field_u64(fields, "num_shreds_received")?,
        num_errors: field_u64(fields, "num_errors").unwrap_or(0),
    })
}

fn parse_blockstore_insert(fields: &str) -> Option<MetricEvent> {
    Some(MetricEvent::BlockstoreInsert {
        num_shreds: field_u64(fields, "num_shreds")?,
        num_inserted: field_u64(fields, "num_inserted").unwrap_or(0),
        num_repair: field_u64(fields, "num_repair").unwrap_or(0),
        num_recovered: field_u64(fields, "num_recovered").unwrap_or(0),
        total_elapsed_us: field_u64(fields, "total_elapsed_us").unwrap_or(0),
    })
}

fn parse_shred_recovery(fields: &str) -> Option<MetricEvent> {
    // Require at least one merkle count to be present. A line without
    // either field is malformed (every observed `shred-recovery`
    // datapoint carries both); returning None is preferable to
    // emitting an all-zero event that aliases with the "no activity"
    // case in downstream aggregation.
    let merkle_code = field_u64(fields, "num_shreds_merkle_code_chained")?;
    Some(MetricEvent::ShredRecovery {
        merkle_code,
        merkle_data: field_u64(fields, "num_shreds_merkle_data_chained").unwrap_or(0),
    })
}

fn parse_shred_insert_is_full(fields: &str) -> Option<MetricEvent> {
    Some(MetricEvent::ShredInsertIsFull {
        slot: field_u64(fields, "slot")?,
        total_time_ms: field_u64(fields, "total_time_ms").unwrap_or(0),
        last_index: field_u64(fields, "last_index").unwrap_or(0),
        num_repaired: field_u64(fields, "num_repaired").unwrap_or(0),
        num_recovered: field_u64(fields, "num_recovered").unwrap_or(0),
    })
}

fn parse_retransmit_first_shred(fields: &str) -> Option<MetricEvent> {
    Some(MetricEvent::RetransmitFirstShred {
        slot: field_u64(fields, "slot")?,
    })
}

fn parse_retransmit_slot_stats(fields: &str) -> Option<MetricEvent> {
    Some(MetricEvent::RetransmitSlotStats {
        slot: field_u64(fields, "slot")?,
        num_shreds: field_u64(fields, "num_shreds").unwrap_or(0),
        num_nodes: field_u64(fields, "num_nodes").unwrap_or(0),
        elapsed_millis: field_u64(fields, "elapsed_millis").unwrap_or(0),
    })
}

fn parse_slot_tracking(fields: &str) -> Option<MetricEvent> {
    Some(MetricEvent::SlotTracking {
        slot: field_u64(fields, "slot")?,
        first_shred_us: field_u64(fields, "first_shred").unwrap_or(0),
        vote_notarize_us: field_u64(fields, "vote_notarize").unwrap_or(0),
        finalized_us: field_u64(fields, "finalized").unwrap_or(0),
        is_fast_finalization: field_bool(fields, "is_fast_finalization").unwrap_or(false),
    })
}

// ---- field extraction primitives -------------------------------------------

/// Locate `key=...` inside a space-separated `key=value` field set.
///
/// Returns the substring of `fields` starting *after* the `=`. The key
/// must be at the very beginning of `fields` or preceded by a space —
/// this prevents `num_shreds` from matching when we asked for `shreds`,
/// or `num_shreds_received` from matching for `num_shreds`.
fn find_field<'a>(fields: &'a str, key: &str) -> Option<&'a str> {
    let needle_first = format!("{key}=");
    if let Some(rest) = fields.strip_prefix(&needle_first) {
        return Some(rest);
    }
    let needle_inner = format!(" {key}=");
    fields
        .find(&needle_inner)
        .map(|i| &fields[i + needle_inner.len()..])
}

/// Parse a leading u64 from `s`, allowing the optional trailing `i`
/// suffix used by InfluxDB line protocol integers (`771i`). Stops at
/// the first non-digit character.
fn parse_leading_u64(s: &str) -> Option<u64> {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    s[..end].parse().ok()
}

/// Read a u64-valued field by key.
fn field_u64(fields: &str, key: &str) -> Option<u64> {
    parse_leading_u64(find_field(fields, key)?)
}

/// Read a boolean-valued field by key. Accepts only literal
/// `true` / `false`; anything else returns `None`.
fn field_bool(fields: &str, key: &str) -> Option<bool> {
    let after = find_field(fields, key)?;
    if after.starts_with("true") {
        Some(true)
    } else if after.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_datapoint_lines_ignored() {
        assert!(parse_body("").is_none());
        assert!(parse_body("arbitrary nonsense body").is_none());
        assert!(parse_body("datapoint: unknown_event x=1i").is_none());
    }

    #[test]
    fn shred_fetch_parses_count() {
        let line = "datapoint: shred_fetch index_overrun=0i shred_count=357i num_shreds_merkle_code_chained=183i";
        let Some(EventKind::Metric(MetricEvent::ShredFetch { shred_count })) = parse_body(line)
        else {
            panic!("expected ShredFetch");
        };
        assert_eq!(shred_count, 357);
    }

    #[test]
    fn shred_fetch_repair_parses_count() {
        let line = "datapoint: shred_fetch_repair shred_count=5i slot_out_of_range=4i";
        let Some(EventKind::Metric(MetricEvent::ShredFetchRepair { shred_count })) =
            parse_body(line)
        else {
            panic!("expected ShredFetchRepair");
        };
        assert_eq!(shred_count, 5);
    }

    #[test]
    fn shred_sigverify_sums_pre_and_post_discards() {
        let line = "datapoint: shred_sigverify num_iters=104i num_batches=104i num_packets=886i num_discards_pre=8i num_deduper_saturations=0i num_discards_post=3i num_duplicates=2i elapsed_micros=14448i";
        let Some(EventKind::Metric(MetricEvent::ShredSigverify {
            num_packets,
            num_discards,
            num_duplicates,
            elapsed_micros,
        })) = parse_body(line)
        else {
            panic!("expected ShredSigverify");
        };
        assert_eq!(num_packets, 886);
        assert_eq!(num_discards, 11);
        assert_eq!(num_duplicates, 2);
        assert_eq!(elapsed_micros, 14448);
    }

    #[test]
    fn recv_window_insert_extracts_received_and_errors() {
        let line = "datapoint: recv-window-insert-shreds num_shreds_received=771i shred_receiver_elapsed_us=1448797i num_errors=3i";
        let Some(EventKind::Metric(MetricEvent::RecvWindowInsert {
            num_shreds_received,
            num_errors,
        })) = parse_body(line)
        else {
            panic!("expected RecvWindowInsert");
        };
        assert_eq!(num_shreds_received, 771);
        assert_eq!(num_errors, 3);
    }

    #[test]
    fn blockstore_insert_captures_partition() {
        let line = "datapoint: blockstore-insert-shreds num_shreds=771i total_elapsed_us=11832i num_inserted=601i num_repair=0i num_recovered=184i num_recovered_inserted=184i";
        let Some(EventKind::Metric(MetricEvent::BlockstoreInsert {
            num_shreds,
            num_inserted,
            num_repair,
            num_recovered,
            total_elapsed_us,
        })) = parse_body(line)
        else {
            panic!("expected BlockstoreInsert");
        };
        assert_eq!(num_shreds, 771);
        assert_eq!(num_inserted, 601);
        assert_eq!(num_repair, 0);
        assert_eq!(num_recovered, 184);
        assert_eq!(total_elapsed_us, 11832);
    }

    #[test]
    fn shred_recovery_captures_merkle_chained_counts() {
        let line = "datapoint: shred-recovery index_overrun=0i shred_count=0i num_shreds_merkle_code_chained=187i num_shreds_merkle_data_chained=184i";
        let Some(EventKind::Metric(MetricEvent::ShredRecovery {
            merkle_code,
            merkle_data,
        })) = parse_body(line)
        else {
            panic!("expected ShredRecovery");
        };
        assert_eq!(merkle_code, 187);
        assert_eq!(merkle_data, 184);
    }

    #[test]
    fn shred_insert_is_full_captures_per_slot_breakdown() {
        let line = "datapoint: shred_insert_is_full slot=2048884i total_time_ms=40i last_index=95i num_repaired=0i num_recovered=44i";
        let Some(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot,
            total_time_ms,
            last_index,
            num_repaired,
            num_recovered,
        })) = parse_body(line)
        else {
            panic!("expected ShredInsertIsFull");
        };
        assert_eq!(slot, 2_048_884);
        assert_eq!(total_time_ms, 40);
        assert_eq!(last_index, 95);
        assert_eq!(num_repaired, 0);
        assert_eq!(num_recovered, 44);
    }

    #[test]
    fn retransmit_first_shred_captures_slot() {
        let line = "datapoint: retransmit-first-shred slot=2048884i";
        let Some(EventKind::Metric(MetricEvent::RetransmitFirstShred { slot })) = parse_body(line)
        else {
            panic!("expected RetransmitFirstShred");
        };
        assert_eq!(slot, 2_048_884);
    }

    #[test]
    fn retransmit_slot_stats_captures_turbine_tree() {
        let line = "datapoint: retransmit-stage-slot-stats slot=2048046i outset_timestamp=1779983655543i elapsed_millis=31i num_shreds=192i num_nodes=91i num_shreds_received_root=1i num_shreds_received_1st_layer=191i";
        let Some(EventKind::Metric(MetricEvent::RetransmitSlotStats {
            slot,
            num_shreds,
            num_nodes,
            elapsed_millis,
        })) = parse_body(line)
        else {
            panic!("expected RetransmitSlotStats");
        };
        assert_eq!(slot, 2_048_046);
        assert_eq!(num_shreds, 192);
        assert_eq!(num_nodes, 91);
        assert_eq!(elapsed_millis, 31);
    }

    #[test]
    fn slot_tracking_captures_timings_and_fast_path() {
        let line = "datapoint: event_handler_slot_tracking slot=2048877i first_shred=0i vote_notarize=86670i finalized=137577i is_fast_finalization=true";
        let Some(EventKind::Metric(MetricEvent::SlotTracking {
            slot,
            first_shred_us,
            vote_notarize_us,
            finalized_us,
            is_fast_finalization,
        })) = parse_body(line)
        else {
            panic!("expected SlotTracking");
        };
        assert_eq!(slot, 2_048_877);
        assert_eq!(first_shred_us, 0);
        assert_eq!(vote_notarize_us, 86670);
        assert_eq!(finalized_us, 137577);
        assert!(is_fast_finalization);
    }

    #[test]
    fn slot_tracking_handles_false_fast_path() {
        let line = "datapoint: event_handler_slot_tracking slot=42i first_shred=10i vote_notarize=20i finalized=30i is_fast_finalization=false";
        let Some(EventKind::Metric(MetricEvent::SlotTracking {
            is_fast_finalization,
            ..
        })) = parse_body(line)
        else {
            panic!("expected SlotTracking");
        };
        assert!(!is_fast_finalization);
    }

    #[test]
    fn find_field_respects_word_boundary() {
        // `num_shreds_received=N` must not match when looking for `num_shreds`.
        let f = "num_shreds_received=771i num_shreds=192i";
        assert_eq!(find_field(f, "num_shreds"), Some("192i"));
        assert_eq!(
            find_field(f, "num_shreds_received"),
            Some("771i num_shreds=192i")
        );
    }

    #[test]
    fn find_field_returns_none_on_missing_key() {
        assert!(find_field("a=1i b=2i", "c").is_none());
    }

    #[test]
    fn parse_leading_u64_strips_i_suffix() {
        assert_eq!(parse_leading_u64("771i num_packets=...").unwrap(), 771);
        assert_eq!(parse_leading_u64("0i extra").unwrap(), 0);
        assert!(parse_leading_u64("not-a-number").is_none());
        assert!(parse_leading_u64("").is_none());
    }
}
