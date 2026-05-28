//! `aggregator::classify_skips` paths: direct-finalize evidence,
//! ancestry-only evidence, indeterminate. Includes a multi-step
//! ancestry chain to confirm the walk doesn't bail short.

use super::super::*;

#[test]
fn classify_skips_covers_all_three_paths() {
    // Reproduces the empirical 1702399 incident shape plus a baseline
    // direct-finalize canonical skip and an indeterminate skip.
    //
    //   1000  voted_skip + finalized   -> CanonicalSkip(DirectFinalize)
    //   2000  voted_skip + no finalize, but 2001 finalized with parent=2000
    //                                  -> CanonicalSkip(Ancestry)
    //   3000  voted_skip + no finalize, no descendant finalized
    //                                  -> Indeterminate
    //   4000  finalized, no skip       -> NotSkipped
    //
    // After classify_skips runs the per-slot field and the overall
    // counters must reflect this partition exactly.
    use crate::model::slot::{CanonicalSkipEvidence, SkipClassification};

    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);

    // Direct-finalize canonical skip: 1000
    {
        let r = state.slot_mut(1000);
        r.voted_skip_at = Some(ts);
        r.finalized_at = Some(ts);
    }
    // Ancestry-only canonical skip: 2000, with 2001 finalized and pointing back.
    {
        let r = state.slot_mut(2000);
        r.voted_skip_at = Some(ts);
    }
    {
        let r = state.slot_mut(2001);
        r.finalized_at = Some(ts);
        r.parent = Some((2000, "hash2000".to_owned()));
    }
    // Indeterminate skip: 3000 — no descendant finalized.
    {
        let r = state.slot_mut(3000);
        r.voted_skip_at = Some(ts);
    }
    // NotSkipped baseline: 4000 — finalized normally.
    {
        let r = state.slot_mut(4000);
        r.finalized_at = Some(ts);
    }

    classify_skips(&mut state);

    assert_eq!(
        state.slots[&1000].skip_classification,
        SkipClassification::CanonicalSkip(CanonicalSkipEvidence::DirectFinalize),
    );
    assert_eq!(
        state.slots[&2000].skip_classification,
        SkipClassification::CanonicalSkip(CanonicalSkipEvidence::Ancestry),
    );
    assert_eq!(
        state.slots[&3000].skip_classification,
        SkipClassification::Indeterminate,
    );
    assert_eq!(
        state.slots[&4000].skip_classification,
        SkipClassification::NotSkipped,
    );

    assert_eq!(state.overall.canonical_skips_direct, 1);
    assert_eq!(state.overall.canonical_skips_ancestry, 1);
    assert_eq!(state.overall.indeterminate_skips, 1);
}

#[test]
fn classify_skips_long_ancestry_chain() {
    // Multi-step ancestry: 5000 is the ancestor of 5005 via a chain of
    // parent pointers 5005 -> 5004 -> ... -> 5000. Only 5005 has a
    // Finalized event observed. The walk should still reach 5000.
    use crate::model::slot::{CanonicalSkipEvidence, SkipClassification};

    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);

    // Set up the chain: parent of N is N-1 for slots 5001..=5005.
    for slot in 5001..=5005u64 {
        let r = state.slot_mut(slot);
        r.parent = Some((slot - 1, format!("hash{}", slot - 1)));
    }
    // Only 5005 is finalized.
    state.slot_mut(5005).finalized_at = Some(ts);
    // 5000 voted skip but never finalized directly.
    state.slot_mut(5000).voted_skip_at = Some(ts);

    classify_skips(&mut state);

    assert_eq!(
        state.slots[&5000].skip_classification,
        SkipClassification::CanonicalSkip(CanonicalSkipEvidence::Ancestry),
    );
    assert_eq!(state.overall.canonical_skips_ancestry, 1);
}
