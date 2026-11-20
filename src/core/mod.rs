//! Core orchestration logic.
//!
//! M2b provides the live mount lifecycle: state registry, reconciliation with
//! the kernel mount table, and the orchestrator that spawns engine processes.

pub mod config_ux;
pub mod creds;
pub mod install;
pub mod orchestrator;
pub mod reconcile;
pub mod state;
