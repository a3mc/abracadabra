//! Command-line argument parsing.

use std::path::PathBuf;

use clap::Parser;

/// Bucket-size floor: smaller than this and per-bucket aggregates become
/// statistically noisy on a 21h log, and bucket counts climb into the
/// thousands for sparkline rendering.
pub const MIN_BUCKET_SECS: i64 = 60;

/// Bucket-size ceiling: beyond 24h we'd have <2 buckets even for the
/// largest logs we expect, defeating the purpose of time-series view.
pub const MAX_BUCKET_SECS: i64 = 24 * 3600;

#[derive(Debug, Parser)]
#[command(
    name = "abracadabra",
    version,
    about = "Solana Alpenglow validator log analyzer",
    long_about = "Streams a saved validator log, parses Alpenglow consensus events, \
                  and presents a per-slot lifecycle view in a terminal UI."
)]
pub struct Cli {
    /// Path to the validator log file.
    #[arg(value_name = "LOG")]
    pub path: PathBuf,

    /// Print a text summary instead of opening the interactive TUI.
    /// (Also auto-enabled when stdout is not a terminal — e.g. piped.)
    #[arg(long, default_value_t = false)]
    pub text: bool,

    /// Bucket size for time-series aggregation. Accepts `30s`, `5m`, `1h`,
    /// etc. Bounds: 1m..=24h. Smaller → finer trend resolution but noisier
    /// per-bucket aggregates and more bars to render. Larger → smoother
    /// trend but loses sub-bucket detail.
    #[arg(
        long,
        default_value = "10m",
        value_name = "DUR",
        value_parser = parse_bucket_duration,
    )]
    pub bucket: i64,
}

/// Parse a duration string of the form `<NN>(s|m|h)` into seconds.
///
/// Empty or unitless input is rejected so we never silently default to
/// seconds for a typo like `--bucket 10` (intent ambiguous).
fn parse_bucket_duration(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".to_owned());
    }
    let split_at = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("missing unit in '{s}' — expected suffix s/m/h"))?;
    let (digits, unit) = s.split_at(split_at);
    if digits.is_empty() {
        return Err(format!("missing number in '{s}'"));
    }
    let n: i64 = digits
        .parse()
        .map_err(|e| format!("bad number in '{s}': {e}"))?;
    let secs = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3600),
        other => {
            return Err(format!("unknown unit '{other}' in '{s}' — expected s/m/h"));
        }
    };
    if secs < MIN_BUCKET_SECS {
        return Err(format!(
            "bucket too small ({secs}s); minimum {MIN_BUCKET_SECS}s"
        ));
    }
    if secs > MAX_BUCKET_SECS {
        return Err(format!(
            "bucket too large ({secs}s); maximum {MAX_BUCKET_SECS}s ({}h)",
            MAX_BUCKET_SECS / 3600
        ));
    }
    Ok(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_minutes() {
        assert_eq!(parse_bucket_duration("10m"), Ok(600));
    }

    #[test]
    fn accepts_seconds() {
        assert_eq!(parse_bucket_duration("60s"), Ok(60));
    }

    #[test]
    fn accepts_hours() {
        assert_eq!(parse_bucket_duration("1h"), Ok(3600));
        assert_eq!(parse_bucket_duration("24h"), Ok(MAX_BUCKET_SECS));
    }

    #[test]
    fn rejects_below_floor() {
        assert!(parse_bucket_duration("10s").is_err());
        assert!(parse_bucket_duration("59s").is_err());
    }

    #[test]
    fn rejects_above_ceiling() {
        assert!(parse_bucket_duration("25h").is_err());
    }

    #[test]
    fn rejects_missing_unit() {
        assert!(parse_bucket_duration("10").is_err());
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(parse_bucket_duration("10d").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_bucket_duration("").is_err());
        assert!(parse_bucket_duration("   ").is_err());
    }
}
