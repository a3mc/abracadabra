//! Real-time log following: detection, tail thread, animated TUI scenes.
//!
//! Stage 1 (LIVE-1): activity detection — decides whether a target log
//! file is currently being written to, so the TUI can gate the Live tab
//! between active and grayed-out states. Subsequent stages add the tail
//! thread (LIVE-3), the animation engine (LIVE-4), and concrete scenes
//! (LIVE-5+).
//!
//! The rest of abracadabra (parser, aggregator, snapshot tabs) is
//! unchanged by anything in this module — the Live tab is an additive
//! surface, not a refactor of existing behaviour.

pub mod detect;
