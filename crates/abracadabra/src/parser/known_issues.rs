//! `solana_core::cluster_slots_service[::cluster_slots]` — the post-migration
//! loose end documented at `docs/alpenglow/investigations/01-cluster-slots-loose-end.md`.
//!
//! Six log lines from this module surface in our analysis:
//!
//! | Line                                                       | Event                       |
//! |------------------------------------------------------------|-----------------------------|
//! | `No epoch_metadata record for epoch N`                     | `NoEpochMetadata`           |
//! | `No epoch info for slot N`                                 | `NoEpochInfoForSlot`        |
//! | `Updating epoch_metadata for epoch N`                      | `UpdatingEpochMetadata`     |
//! | `Evicting epoch_metadata for epoch N`                      | `EvictingEpochMetadata`     |
//! | `Invalid update call to ClusterSlots, can not roll ...`    | `InvalidClusterSlotsUpdate` |
//! | `ClusterSlotsService has stopped because we have finished` | `ClusterSlotsStopped`       |

use crate::parser::EventKind;

pub fn parse_body(body: &str) -> Option<EventKind> {
    if let Some(rest) = body.strip_prefix("No epoch_metadata record for epoch ") {
        Some(EventKind::NoEpochMetadata {
            epoch: rest.parse().ok()?,
        })
    } else if let Some(rest) = body.strip_prefix("No epoch info for slot ") {
        Some(EventKind::NoEpochInfoForSlot {
            slot: rest.parse().ok()?,
        })
    } else if let Some(rest) = body.strip_prefix("Updating epoch_metadata for epoch ") {
        Some(EventKind::UpdatingEpochMetadata {
            epoch: rest.parse().ok()?,
        })
    } else if let Some(rest) = body.strip_prefix("Evicting epoch_metadata for epoch ") {
        Some(EventKind::EvictingEpochMetadata {
            epoch: rest.parse().ok()?,
        })
    } else if body.starts_with("Invalid update call to ClusterSlots") {
        Some(EventKind::InvalidClusterSlotsUpdate)
    } else if body.starts_with(
        "ClusterSlotsService has stopped because we have finished the alpenglow migration epoch",
    ) {
        Some(EventKind::ClusterSlotsStopped)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_epoch_metadata() {
        let ev = parse_body("No epoch_metadata record for epoch 19").unwrap();
        assert!(matches!(ev, EventKind::NoEpochMetadata { epoch: 19 }));
    }

    #[test]
    fn no_epoch_info_for_slot() {
        let ev = parse_body("No epoch info for slot 1234567").unwrap();
        assert!(matches!(
            ev,
            EventKind::NoEpochInfoForSlot { slot: 1234567 }
        ));
    }

    #[test]
    fn updating_epoch_metadata() {
        let ev = parse_body("Updating epoch_metadata for epoch 22").unwrap();
        assert!(matches!(ev, EventKind::UpdatingEpochMetadata { epoch: 22 }));
    }

    #[test]
    fn evicting_epoch_metadata() {
        let ev = parse_body("Evicting epoch_metadata for epoch 21").unwrap();
        assert!(matches!(ev, EventKind::EvictingEpochMetadata { epoch: 21 }));
    }

    #[test]
    fn invalid_update_call() {
        let ev = parse_body("Invalid update call to ClusterSlots, can not roll time backwards!")
            .unwrap();
        assert!(matches!(ev, EventKind::InvalidClusterSlotsUpdate));
    }

    #[test]
    fn cluster_slots_stopped() {
        let ev = parse_body(
            "ClusterSlotsService has stopped because we have finished the alpenglow migration epoch",
        )
        .unwrap();
        assert!(matches!(ev, EventKind::ClusterSlotsStopped));
    }

    #[test]
    fn unrelated_body_is_none() {
        assert!(parse_body("Some other cluster log line").is_none());
    }
}
