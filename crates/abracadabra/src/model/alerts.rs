//! Anomaly surface: typed alerts derived during aggregation.

use time::OffsetDateTime;

/// Variant order is significant: derived `Ord` returns
/// `Info < Warn < Critical`, matching the alert ranking the surface
/// functions rely on (Critical-first sort).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertKind {
    /// Validator was in `FullAlpenglowEpoch` (ClusterSlotsService has stopped).
    /// Informational marker, NOT a problem on its own.
    ClusterSlotsShutdownObserved,

    /// Standstill firing — finalization stalled for ≥10s.
    StandstillObserved { at_slot: u64 },

    /// Repeated WARN/ERROR lines from a single module that no dedicated
    /// parser recognises. Aggregated by `(severity, module)` so noisy
    /// streams collapse to one alert with a count.
    LogPattern {
        severity: Severity,
        module: String,
        count: u64,
    },

    /// Local validator's leader-window summary — once per log, INFO.
    /// `slot_count` is the total number of slots we were leader for;
    /// `window_count` is the number of `ProduceWindow` announcements.
    LocalLeaderSummary { slot_count: u64, window_count: u64 },

    /// `agave_votor::event_handler] Set identity` line — operator
    /// rotated validator identity. INFO marker; not a problem on its
    /// own but worth surfacing as a timeline anchor.
    IdentityChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub severity: Severity,
    pub at: OffsetDateTime,
    pub kind: AlertKind,
    pub description: String,
}

impl Alert {
    pub const fn new(
        severity: Severity,
        at: OffsetDateTime,
        kind: AlertKind,
        description: String,
    ) -> Self {
        Self {
            severity,
            at,
            kind,
            description,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn alert_construct() {
        let a = Alert::new(
            Severity::Warn,
            datetime!(2026-05-24 13:06:32 UTC),
            AlertKind::StandstillObserved { at_slot: 1207084 },
            "stuck".to_owned(),
        );
        assert_eq!(a.severity, Severity::Warn);
        assert!(matches!(
            a.kind,
            AlertKind::StandstillObserved { at_slot: 1207084 }
        ));
    }
}
