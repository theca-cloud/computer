//! The Theca sync client: local folders kept identical to libraries, both
//! ways (docs/decisions.md, Sync; ADR 0004).

pub mod engine;
pub mod exec;
#[cfg(feature = "gui")]
pub mod gui;
pub mod local;
pub mod ops;
pub mod plan;
pub mod state;
