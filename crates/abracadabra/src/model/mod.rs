//! In-memory data model populated by the aggregator.
//!
//! - `slot`   — per-slot lifecycle record
//! - `state`  — top-level state aggregating all slots, counters, alerts
//! - `alerts` — typed anomalies surfaced from the event stream

pub mod alerts;
pub mod analysis;
pub mod buckets;
pub mod slot;
pub mod state;
pub mod window;
