//! `agave_votor::root_utils` — `setting root` and `new root` log lines.
//!
//! Both are emitted with the standard `<pubkey>: <event-text>` shape.

use crate::parser::EventKind;

/// Parse the body of an `agave_votor::root_utils` info-log line.
pub fn parse_body(body: &str) -> Option<EventKind> {
    let (_pubkey, event) = body.split_once(": ")?;
    if let Some(slot) = event.strip_prefix("setting root ") {
        Some(EventKind::SettingRoot {
            slot: slot.parse().ok()?,
        })
    } else if let Some(slot) = event.strip_prefix("new root ") {
        Some(EventKind::NewRoot {
            slot: slot.parse().ok()?,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PK: &str = "ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu";

    fn body(s: &str) -> String {
        format!("{PK}: {s}")
    }

    #[test]
    fn setting_root() {
        let ev = parse_body(&body("setting root 1207084")).unwrap();
        assert!(matches!(ev, EventKind::SettingRoot { slot: 1207084 }));
    }

    #[test]
    fn new_root() {
        let ev = parse_body(&body("new root 1207084")).unwrap();
        assert!(matches!(ev, EventKind::NewRoot { slot: 1207084 }));
    }

    #[test]
    fn unrelated_body_is_none() {
        assert!(parse_body(&body("something else 42")).is_none());
    }

    #[test]
    fn missing_pubkey_is_none() {
        // No `<pubkey>: ` prefix.
        assert!(parse_body("setting root 1234").is_none());
    }
}
