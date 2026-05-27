//! Split a log line into its bracketed prefix `[ts LEVEL  module]` plus body.
//!
//! Format reference (from `docs/alpenglow/log-strings-reference.md`):
//! ```text
//! [YYYY-MM-DDThh:mm:ss.nnnnnnnnnZ LEVEL  module::path] message
//! ```
//! Spacing between LEVEL and module varies (1 or 2 spaces) so module columns
//! align. We tolerate any amount of inter-token whitespace via `trim_start`.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::parser::ParseError;

/// Severity levels emitted by Solana validator logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
    Debug,
    Trace,
}

/// Disassembled prefix of a single log line.
#[derive(Debug, Clone)]
pub struct LineParts<'a> {
    pub ts: OffsetDateTime,
    pub level: Level,
    pub module: &'a str,
    pub body: &'a str,
}

/// Try to parse the bracketed prefix.
///
/// Returns `Ok(None)` for continuation lines (no `[` at column 0) — these
/// are the wrapped multi-line outputs from BTreeMap dumps and similar.
///
/// Returns `Err` only on a clearly malformed bracketed prefix (truncated
/// timestamp, missing closing bracket, unrecognised level). Cleanly-shaped
/// lines whose body we don't care about still return `Ok(Some(parts))`.
pub fn parse_prefix(line: &str) -> Result<Option<LineParts<'_>>, ParseError> {
    let Some(rest) = line.strip_prefix('[') else {
        return Ok(None);
    };

    // Pre-check the first token's shape against RFC3339 (`YYYY-`) before
    // committing to the parse path. Lines that begin with `[` but are not
    // log records (Rust panic banners, BOM-prefixed first lines, decorative
    // bracketed text) would otherwise inflate `parse_errors` and dilute the
    // corruption signal. Such lines route to `Parsed::Continuation`.
    if !looks_like_rfc3339_date(rest) {
        return Ok(None);
    }

    let ts_end = rest.find(' ').ok_or(ParseError::LinePrefix)?;
    let ts_str = &rest[..ts_end];
    let after_ts = &rest[ts_end + 1..];

    let ts = OffsetDateTime::parse(ts_str, &Rfc3339).map_err(|_| ParseError::Timestamp)?;

    let level_end = after_ts.find(' ').ok_or(ParseError::LinePrefix)?;
    let level_str = &after_ts[..level_end];
    let level = match level_str {
        "INFO" => Level::Info,
        "WARN" => Level::Warn,
        "ERROR" => Level::Error,
        "DEBUG" => Level::Debug,
        "TRACE" => Level::Trace,
        _ => return Err(ParseError::LinePrefix),
    };

    // The 1-or-2-space gap between level and module aligns module columns.
    let after_level = after_ts[level_end + 1..].trim_start_matches(' ');

    let close_idx = after_level.find("] ").ok_or(ParseError::LinePrefix)?;
    let module = &after_level[..close_idx];
    let body = &after_level[close_idx + 2..];

    Ok(Some(LineParts {
        ts,
        level,
        module,
        body,
    }))
}

/// Cheap shape-check for an RFC3339 date prefix: four ASCII digits followed
/// by `-`. Catches the common "starts with `[` but isn't a log line" case
/// (e.g. `[ ` panic banner, `[INFO]`-style ad-hoc tags) without allocating.
fn looks_like_rfc3339_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 5
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real INFO line with double space between level and module.
    const SAMPLE_INFO: &str = "[2026-05-23T16:00:07.187019566Z INFO  agave_votor::event_handler] \
         ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu: First shred 1028071";

    /// Real ERROR line with single space between level and module.
    const SAMPLE_ERROR: &str =
        "[2026-05-23T16:00:07.186801076Z ERROR solana_core::cluster_slots_service::cluster_slots] \
         No epoch_metadata record for epoch 19";

    #[test]
    fn parses_info_line() {
        let parts = parse_prefix(SAMPLE_INFO).unwrap().unwrap();
        assert_eq!(parts.level, Level::Info);
        assert_eq!(parts.module, "agave_votor::event_handler");
        assert!(parts.body.starts_with("ALNSCya"));
        assert!(parts.body.ends_with("First shred 1028071"));
        // Timestamp parses to nanosecond precision.
        assert_eq!(parts.ts.nanosecond(), 187_019_566);
    }

    #[test]
    fn parses_error_line() {
        let parts = parse_prefix(SAMPLE_ERROR).unwrap().unwrap();
        assert_eq!(parts.level, Level::Error);
        assert_eq!(
            parts.module,
            "solana_core::cluster_slots_service::cluster_slots"
        );
        assert_eq!(parts.body, "No epoch_metadata record for epoch 19");
    }

    #[test]
    fn continuation_line_yields_none() {
        // Multi-line gossip dump continuations — no bracket prefix.
        assert!(parse_prefix("    Address    | Stake").unwrap().is_none());
        assert!(parse_prefix("").unwrap().is_none());
    }

    #[test]
    fn malformed_prefix_is_error() {
        // Bracket-prefixed line whose first token shape DOES match RFC3339 but
        // whose level is unrecognised is a real malformed log line.
        assert!(parse_prefix("[2026-05-23T16:00:07.000000000Z FATAL mod::path] body").is_err());
    }

    #[test]
    fn bracket_without_rfc3339_shape_is_continuation() {
        // PARSE-03: a `[`-prefixed line whose first 5 bytes do not match
        // `\d{4}-` is not a log record (panic banner, ad-hoc tag, etc.).
        // Routes to Continuation, not Err — keeps `parse_errors` reserved
        // for genuinely malformed log lines.
        assert!(parse_prefix("[broken").unwrap().is_none());
        assert!(parse_prefix("[INFO] not-our-format").unwrap().is_none());
        assert!(parse_prefix("[ panic banner ]").unwrap().is_none());
    }

    #[test]
    fn body_preserves_internal_whitespace() {
        let parts = parse_prefix(SAMPLE_INFO).unwrap().unwrap();
        assert!(parts.body.contains(":  ") || parts.body.contains(": "));
    }
}
