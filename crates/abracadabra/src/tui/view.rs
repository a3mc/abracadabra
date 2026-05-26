//! Pre-computed per-row data for the Slots and Recoveries tables.
//!
//! Computing these once at App startup keeps the per-frame render cheap
//! (`Table` only needs the visible slice, but we still want to avoid
//! recomputing latencies on every keystroke).

use crate::model::analysis::VoteResumeRecord;
use crate::model::slot::{SlotRecord, SlotStatus};

#[derive(Debug, Clone)]
pub struct SlotViewRow {
    pub slot: u64,
    pub status: SlotStatus,
    pub fast: Option<bool>,
    pub we_are_leader: bool,
    /// `first_shred_at` → `block_emitted_at` (shred reception + replay).
    pub assembly_ms: Option<f64>,
    /// `block_emitted_at` → `finalized_at` (pure consensus rounds).
    pub consensus_ms: Option<f64>,
    /// `first_shred_at` → `finalized_at` (full lifecycle = assembly + consensus).
    pub lifecycle_ms: Option<f64>,
    pub voted_notarize: bool,
    pub voted_finalize: bool,
    pub voted_skip: bool,
    pub safe_to_notar: bool,
    pub safe_to_skip: bool,
    pub crashed_leader: bool,
}

impl SlotViewRow {
    pub fn from_record(r: &SlotRecord) -> Self {
        let to_ms = |us: i64| us as f64 / 1000.0;
        let assembly_ms = SlotRecord::delta_us(r.first_shred_at, r.block_emitted_at).map(to_ms);
        let consensus_ms = SlotRecord::delta_us(r.block_emitted_at, r.finalized_at).map(to_ms);
        let lifecycle_ms = SlotRecord::delta_us(r.first_shred_at, r.finalized_at).map(to_ms);
        Self {
            slot: r.slot,
            status: r.status(),
            fast: r.fast_finalize,
            we_are_leader: r.we_are_leader,
            assembly_ms,
            consensus_ms,
            lifecycle_ms,
            voted_notarize: r.voted_notarize_at.is_some(),
            voted_finalize: r.voted_finalize_at.is_some(),
            voted_skip: r.voted_skip_at.is_some(),
            safe_to_notar: r.safe_to_notar_at.is_some(),
            safe_to_skip: r.safe_to_skip_at.is_some(),
            crashed_leader: r.timeout_crashed_leader_at.is_some(),
        }
    }

    pub const fn vote_pattern(&self) -> &'static str {
        match (self.voted_notarize, self.voted_finalize, self.voted_skip) {
            (true, true, _) => "N+F",
            (true, false, _) => "N",
            (false, _, true) => "S",
            _ => "-",
        }
    }

    pub const fn status_str(&self) -> &'static str {
        match self.status {
            SlotStatus::FastFinalized | SlotStatus::SlowFinalized => "FIN",
            SlotStatus::Skipped => "SKIP",
            SlotStatus::Pending => "PEND",
        }
    }

    pub const fn fast_str(&self) -> &'static str {
        match (self.status, self.fast) {
            (SlotStatus::FastFinalized, _) | (_, Some(true)) => "F",
            (SlotStatus::SlowFinalized, _) | (_, Some(false)) => "s",
            _ => " ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VoteResumeViewRow {
    pub tcl_slot: u64,
    pub resume_slot: u64,
    pub resume_us: i64,
    pub slot_gap: u64,
}

impl VoteResumeViewRow {
    pub const fn from_record(r: VoteResumeRecord) -> Self {
        let slot_gap = r.resume_slot.saturating_sub(r.tcl_slot);
        Self {
            tcl_slot: r.tcl_slot,
            resume_slot: r.resume_slot,
            resume_us: r.resume_us,
            slot_gap,
        }
    }
}
