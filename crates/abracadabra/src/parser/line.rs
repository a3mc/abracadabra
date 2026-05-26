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

    let ts_end = rest
        .find(' ')
        .ok_or_else(|| ParseError::LinePrefix(line.to_owned()))?;
    let ts_str = &rest[..ts_end];
    let after_ts = &rest[ts_end + 1..];

    let ts = OffsetDateTime::parse(ts_str, &Rfc3339)
        .map_err(|_| ParseError::Timestamp(ts_str.to_owned()))?;

    let level_end = after_ts
        .find(' ')
        .ok_or_else(|| ParseError::LinePrefix(line.to_owned()))?;
    let level_str = &after_ts[..level_end];
    let level = match level_str {
        "INFO" => Level::Info,
        "WARN" => Level::Warn,
        "ERROR" => Level::Error,
        "DEBUG" => Level::Debug,
        "TRACE" => Level::Trace,
        _ => return Err(ParseError::LinePrefix(line.to_owned())),
    };

    // The 1-or-2-space gap between level and module aligns module columns.
    let after_level = after_ts[level_end + 1..].trim_start_matches(' ');

    let close_idx = after_level
        .find("] ")
        .ok_or_else(|| ParseError::LinePrefix(line.to_owned()))?;
    let module = &after_level[..close_idx];
    let body = &after_level[close_idx + 2..];

    Ok(Some(LineParts {
        ts,
        level,
        module,
        body,
    }))
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
        // Bracket but no timestamp.
        assert!(parse_prefix("[broken").is_err());
        // Unrecognised level.
        assert!(parse_prefix("[2026-05-23T16:00:07.000000000Z FATAL mod::path] body").is_err());
    }

    #[test]
    fn body_preserves_internal_whitespace() {
        let parts = parse_prefix(SAMPLE_INFO).unwrap().unwrap();
        assert!(parts.body.contains(":  ") || parts.body.contains(": "));
    }
}
