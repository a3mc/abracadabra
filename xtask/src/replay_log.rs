//! Replay a captured Solana log into a sink file at original temporal pacing.
//!
//! Reads `SOURCE` line-by-line, parses the ISO-8601 nanosecond timestamp at
//! the start of each line, and appends to `DEST` after sleeping for the
//! inter-line delta divided by `speed`. Used to drive the Live tab during
//! local development without needing a real validator server.
//!
//! Lines without a parseable timestamp (continuation lines, blanks) are
//! emitted immediately. Out-of-order timestamps emit immediately (no
//! negative sleep). The sink is truncated on start so a fresh tail
//! begins from byte 0; the Live tab can re-open it as needed.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use time::format_description::well_known::Iso8601;
use time::OffsetDateTime;

/// CLI entry point for `cargo xtask replay-log SOURCE DEST [SPEED]`.
///
/// `SPEED` defaults to `1.0`. Speed must be > 0; `0` or negative is
/// rejected. Speed `10` replays a 24h log in 2.4h. Speed `0.5` halves
/// playback (useful for slow-motion inspection of a busy window).
pub fn run_replay_log(args: &[String]) -> Result<(), String> {
    let source = args.first().ok_or_else(|| {
        "missing SOURCE path. Usage: xtask replay-log SOURCE DEST [SPEED]".to_string()
    })?;
    let dest = args.get(1).ok_or_else(|| {
        "missing DEST path. Usage: xtask replay-log SOURCE DEST [SPEED]".to_string()
    })?;
    let speed: f64 = match args.get(2) {
        Some(s) => s
            .parse()
            .map_err(|e| format!("SPEED parse error '{s}': {e}"))?,
        None => 1.0,
    };
    if !speed.is_finite() || speed <= 0.0 {
        return Err(format!(
            "SPEED must be a positive finite number, got {speed}"
        ));
    }

    let source_path = PathBuf::from(source);
    let dest_path = PathBuf::from(dest);

    let source_file = File::open(&source_path)
        .map_err(|e| format!("open SOURCE {}: {e}", source_path.display()))?;
    let reader = BufReader::with_capacity(64 * 1024, source_file);

    // Truncate the sink on start. Live-tab tail re-opens by inode so
    // truncation here gives consumers a clean starting point.
    let mut dest_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&dest_path)
        .map_err(|e| format!("open DEST {}: {e}", dest_path.display()))?;

    println!(
        "[replay-log] SOURCE={} DEST={} speed={}x",
        source_path.display(),
        dest_path.display(),
        speed
    );

    let started = Instant::now();
    let mut prev_ts: Option<OffsetDateTime> = None;
    let mut line_count: u64 = 0;
    let mut report_at_count: u64 = 1000;

    for line in reader.lines() {
        let line = line.map_err(|e| format!("read SOURCE line {}: {e}", line_count + 1))?;
        line_count = line_count.saturating_add(1);

        // Parse the ISO-8601 timestamp inside the leading `[...]`. Any
        // failure means we treat the line as a continuation; emit it
        // immediately without disturbing the previous pacing anchor.
        let parsed_ts = extract_iso_timestamp(&line);
        if let (Some(curr), Some(prev)) = (parsed_ts, prev_ts) {
            let delta = curr - prev;
            let delta_ns = delta.whole_nanoseconds();
            if delta_ns > 0 {
                #[allow(clippy::cast_precision_loss)]
                let sleep_ns = (delta_ns as f64) / speed;
                if sleep_ns >= 1.0 {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let dur = Duration::from_nanos(sleep_ns as u64);
                    thread::sleep(dur);
                }
            }
        }
        if let Some(ts) = parsed_ts {
            prev_ts = Some(ts);
        }

        writeln!(dest_file, "{line}").map_err(|e| format!("write DEST: {e}"))?;
        dest_file.flush().map_err(|e| format!("flush DEST: {e}"))?;

        if line_count == report_at_count {
            let elapsed = started.elapsed().as_secs_f64();
            #[allow(clippy::cast_precision_loss)]
            let lps = if elapsed > 0.0 {
                line_count as f64 / elapsed
            } else {
                0.0
            };
            println!("[replay-log] {line_count} lines streamed ({lps:.0} lps wall)");
            report_at_count = report_at_count.saturating_mul(2);
        }
    }

    println!(
        "[replay-log] done: {line_count} lines in {:.1}s wall",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Extract the ISO-8601 nanosecond timestamp from a log line.
///
/// Solana lines look like `[2026-05-28T16:00:06.987212241Z INFO  ...]`.
/// The timestamp is the substring between `[` and the first space.
/// Returns `None` for lines that do not start with a parseable
/// timestamp (continuation lines, blanks, etc.).
fn extract_iso_timestamp(line: &str) -> Option<OffsetDateTime> {
    let rest = line.strip_prefix('[')?;
    let end = rest.find(' ')?;
    OffsetDateTime::parse(&rest[..end], &Iso8601::DEFAULT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_timestamp_from_typical_line() {
        let line = "[2026-05-28T16:00:06.987212241Z INFO  solana_runtime::bank] bank frozen: 1";
        let ts = extract_iso_timestamp(line).unwrap();
        assert_eq!(ts.year(), 2026);
        assert_eq!(ts.month() as u8, 5);
        assert_eq!(ts.day(), 28);
        assert_eq!(ts.hour(), 16);
        assert_eq!(ts.second(), 6);
    }

    #[test]
    fn extract_timestamp_returns_none_on_continuation() {
        // Continuation lines don't have a leading `[YYYY-...`.
        assert!(extract_iso_timestamp("  some indented detail").is_none());
        assert!(extract_iso_timestamp("").is_none());
        assert!(extract_iso_timestamp("[malformed").is_none());
    }
}
