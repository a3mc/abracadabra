//! Decide whether a log file is being written to right now.
//!
//! Three signals, evaluated cheapest-first so an obvious "static" answer
//! short-circuits the I/O:
//!
//! 1. Filename pattern — rotation suffixes (`.log.N`, `.log.gz`,
//!    `.log.YYYY-MM-DD`, `.zst`, etc.) prove the file is frozen. No
//!    syscalls required.
//! 2. mtime freshness — stat once. If the last write is older than
//!    [`MTIME_FRESHNESS_THRESHOLD`], the file is stale; no point in
//!    polling for growth.
//! 3. Size delta — sample file length [`SIZE_POLL_SAMPLES`] times across
//!    [`SIZE_POLL_WINDOW`]. Strict growth between any adjacent samples
//!    proves the file is being appended to.
//!
//! All three must agree for [`classify`] to return [`Activity::Active`].
//! mtime alone lies (a freshly downloaded file has a current mtime);
//! filename alone is a heuristic; size delta is the only proof.

use std::fs;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

/// Classification of a target log file at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// File is being appended to in real time. Live tab is enabled.
    Active,
    /// File is frozen, rotated, or otherwise stale. Live tab is grayed.
    /// `StaticReason` records which signal disqualified it (for the
    /// placeholder text shown on the Live tab).
    Static(StaticReason),
}

/// Reason a path failed the activity check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticReason {
    /// Filename matches a known rotation pattern (compressed archive
    /// or `.log.<suffix>` form).
    RotatedFilename,
    /// File's mtime is older than [`MTIME_FRESHNESS_THRESHOLD`].
    StaleMtime { age_secs: u64 },
    /// File size did not strictly increase during the polling window.
    NoSizeGrowth,
}

/// Maximum age of mtime that still qualifies as "fresh". A live
/// Solana validator emits multiple log lines per second; an mtime
/// older than this strongly suggests the file is not being written.
pub const MTIME_FRESHNESS_THRESHOLD: Duration = Duration::from_secs(30);

/// Total wall-clock span over which size deltas are observed.
pub const SIZE_POLL_WINDOW: Duration = Duration::from_millis(2100);

/// Number of size samples taken during [`SIZE_POLL_WINDOW`].
pub const SIZE_POLL_SAMPLES: u32 = 3;

/// Classify `path` as `Active` or `Static`.
///
/// Performs the three checks in order of increasing cost; returns at
/// the first disqualifying signal. The size-delta poll is only entered
/// if the filename and mtime checks already passed, so static files
/// cost a single `stat`.
pub fn classify(path: &Path) -> io::Result<Activity> {
    classify_with_clock(path, SystemTime::now)
}

/// Inner [`classify`] with an injectable clock for tests.
fn classify_with_clock<F>(path: &Path, clock: F) -> io::Result<Activity>
where
    F: Fn() -> SystemTime,
{
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if is_rotated_filename(name) {
            return Ok(Activity::Static(StaticReason::RotatedFilename));
        }
    }

    let metadata = fs::metadata(path)?;
    let modified = metadata.modified()?;
    let now = clock();

    // `duration_since` errors if `modified` is in the future (clock skew
    // or NTP correction). Treat future mtime as fresh — the file was
    // just touched, even if by a wonky clock.
    if let Ok(age) = now.duration_since(modified) {
        if age > MTIME_FRESHNESS_THRESHOLD {
            return Ok(Activity::Static(StaticReason::StaleMtime {
                age_secs: age.as_secs(),
            }));
        }
    }

    if poll_size_growth(path, SIZE_POLL_WINDOW, SIZE_POLL_SAMPLES)? {
        Ok(Activity::Active)
    } else {
        Ok(Activity::Static(StaticReason::NoSizeGrowth))
    }
}

/// True iff `name` looks like a rotated / archived log. Two patterns:
///
/// - Compressed extensions: `.gz`, `.bz2`, `.xz`, `.zst`. These cannot
///   be tailed even if mtime is fresh.
/// - Contains `.log.` followed by any non-empty suffix
///   (`validator.log.1`, `validator.log.20260601`, `validator.log.gz`).
///   Bare `.log` (or no extension) is not rotated by this rule.
fn is_rotated_filename(name: &str) -> bool {
    if let Some(ext) = name.rsplit('.').next() {
        if matches!(ext, "gz" | "bz2" | "xz" | "zst") {
            return true;
        }
    }
    if let Some((_before, after)) = name.rsplit_once(".log.") {
        return !after.is_empty();
    }
    false
}

/// Sample `path`'s length `samples` times, evenly spaced across
/// `window`. Returns true on the first strict increase between two
/// consecutive samples.
fn poll_size_growth(path: &Path, window: Duration, samples: u32) -> io::Result<bool> {
    if samples < 2 {
        return Ok(false);
    }
    let interval = window / samples.saturating_sub(1);
    let mut prev = fs::metadata(path)?.len();
    for _ in 1..samples {
        thread::sleep(interval);
        let next = fs::metadata(path)?.len();
        if next > prev {
            return Ok(true);
        }
        prev = next;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ---- is_rotated_filename ------------------------------------------------

    #[test]
    fn rotated_numeric_suffix() {
        assert!(is_rotated_filename("validator.log.1"));
        assert!(is_rotated_filename("validator.log.42"));
        assert!(is_rotated_filename("solana-validator.log.7"));
    }

    #[test]
    fn rotated_dated_suffix() {
        assert!(is_rotated_filename("validator.log.20260601"));
        assert!(is_rotated_filename("validator.log.2026-06-01"));
    }

    #[test]
    fn rotated_compressed() {
        assert!(is_rotated_filename("validator.log.gz"));
        assert!(is_rotated_filename("validator.log.1.gz"));
        assert!(is_rotated_filename("validator.log.zst"));
        assert!(is_rotated_filename("validator.log.bz2"));
        assert!(is_rotated_filename("validator.log.xz"));
    }

    #[test]
    fn live_filenames_accepted() {
        assert!(!is_rotated_filename("validator.log"));
        assert!(!is_rotated_filename("solana-validator.log"));
        assert!(!is_rotated_filename("validator"));
        assert!(!is_rotated_filename("agave.log"));
    }

    #[test]
    fn empty_after_log_dot_is_not_rotated() {
        // `foo.log.` would be a weird name but technically the suffix
        // after the second dot is empty, so we do NOT call it rotated.
        assert!(!is_rotated_filename("foo.log."));
    }

    // ---- mtime branch in classify_with_clock --------------------------------

    #[test]
    fn stale_mtime_short_circuits() {
        let tmpdir = std::env::temp_dir().join("abr-live-test-stale");
        fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("active.log");
        fs::write(&path, "anything\n").unwrap();

        // Pretend "now" is one hour ahead of the real mtime.
        let real_now = SystemTime::now();
        let future = real_now + Duration::from_secs(3600);

        let result = classify_with_clock(&path, || future).unwrap();
        match result {
            Activity::Static(StaticReason::StaleMtime { age_secs }) => {
                assert!(age_secs >= 3590);
            }
            other => panic!("expected StaleMtime, got {other:?}"),
        }

        fs::remove_file(&path).ok();
    }

    // ---- size-growth poll ---------------------------------------------------

    #[test]
    fn size_growth_detected_when_file_appended() {
        let tmpdir = std::env::temp_dir().join("abr-live-test-grow");
        fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("grow.log");
        fs::write(&path, "initial\n").unwrap();

        let appender = {
            let path = path.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(400));
                let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
                writeln!(f, "appended").unwrap();
            })
        };

        let result = poll_size_growth(&path, Duration::from_millis(1500), 3).unwrap();
        appender.join().unwrap();
        assert!(result, "expected size growth to be observed");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn size_growth_not_detected_on_static_file() {
        let tmpdir = std::env::temp_dir().join("abr-live-test-static");
        fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("static.log");
        fs::write(&path, "frozen\n").unwrap();

        let result = poll_size_growth(&path, Duration::from_millis(600), 3).unwrap();
        assert!(!result, "static file must not register as growing");

        fs::remove_file(&path).ok();
    }

    // ---- full classify pipeline ---------------------------------------------

    #[test]
    fn rotated_filename_short_circuits_pipeline() {
        let tmpdir = std::env::temp_dir().join("abr-live-test-rot");
        fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("validator.log.3");
        fs::write(&path, "rotated content\n").unwrap();

        let result = classify(&path).unwrap();
        assert_eq!(result, Activity::Static(StaticReason::RotatedFilename));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn fresh_mtime_but_no_growth_is_static() {
        let tmpdir = std::env::temp_dir().join("abr-live-test-fresh-static");
        fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("validator.log");
        fs::write(&path, "just written, but won't grow\n").unwrap();

        let result = classify(&path).unwrap();
        assert_eq!(result, Activity::Static(StaticReason::NoSizeGrowth));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn growing_live_file_is_active() {
        let tmpdir = std::env::temp_dir().join("abr-live-test-active");
        fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("validator.log");
        fs::write(&path, "seed\n").unwrap();

        let appender = {
            let path = path.clone();
            thread::spawn(move || {
                for _ in 0..5 {
                    thread::sleep(Duration::from_millis(300));
                    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
                    writeln!(f, "tick").unwrap();
                }
            })
        };

        let result = classify(&path).unwrap();
        appender.join().unwrap();
        assert_eq!(result, Activity::Active);

        fs::remove_file(&path).ok();
    }
}
