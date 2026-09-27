//! Performance testing for Kraken PDF. Nothing here ships in the app.
//!
//! - `perf-runner` (src/main.rs) measures the engine and the real app process against the
//!   budgets in `perf/budgets.toml`.
//! - `benches/` holds criterion micro-benchmarks (`cargo bench -p perf`).

pub mod budgets;
pub mod fixtures;
pub mod report;
pub mod stats;
