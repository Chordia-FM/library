//! Chordia self-hosted library server.
//!
//! Split into a lib + bin so integration tests can drive the real modules. As a binary-only crate
//! nothing under `tests/` could reach any of this — every module was unreachable from outside
//! `main.rs`, which is why the crate had inline unit tests but no integration surface at all.
//!
//! `main.rs` keeps only the boot sequence; everything it orchestrates lives here.

#![allow(dead_code)]

pub mod acquisition;
pub mod api;
pub mod auth;
pub mod catalog;
pub mod catalog_sync;
pub mod config;
pub mod dedupe;
pub mod directory;
pub mod error;
pub mod fingerprint;
pub mod http;
pub mod index;
pub mod loudness;
pub mod metadata;
pub mod organize;
pub mod pairing;
pub mod playback;
pub mod relay;
pub mod scanner;
pub mod scrobble;
pub mod streaming;
pub mod telemetry;
pub mod tls;
pub mod transcode;
