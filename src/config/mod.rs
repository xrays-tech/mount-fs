//! Configuration loading and persistence.
//!
//! The schema lives in [`schema`]; this module re-exports its public API so
//! the rest of the crate can use `crate::config::*`.

mod schema;

pub use schema::*;
