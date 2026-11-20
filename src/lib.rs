//! `mount-fs` library.
//!
//! A Linux CLI for mounting cloud drives by orchestrating existing FUSE tools
//! (s3fs-fuse, aliyundrive-fuse, qrypt). The binary in `src/main.rs` is a thin
//! CLI wrapper around the modules exported here.

pub mod cli;
pub mod config;
pub mod core;
pub mod engines;
pub mod error;
pub mod providers;
pub mod system;
