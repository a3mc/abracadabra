//! abracadabra — Solana Alpenglow validator log analyzer.
//!
//! Library entry: exposes the parser, model, and aggregator modules so that
//! the binary in `main.rs` and the integration test suite can share code.

// Per CLAUDE.md: production code bans `unwrap()` / `expect()` and
// favours readable numeric literals, but test code is explicitly exempt
// from these (tests assert on raw slot numbers, exact f64 comparisons,
// etc., and need to panic on unexpected input). Strict CI runs
// `cargo clippy --all-targets -- -D warnings` which compiles tests AND
// would deny those warnings. Keep the production rule in force; opt
// tests out at the crate root.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unreadable_literal,
        clippy::float_cmp,
    )
)]

pub mod aggregator;
pub mod cli;
pub mod model;
pub mod parser;
pub mod runner;
pub mod tui;
