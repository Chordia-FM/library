//! HTTP middleware.
//!
//! Capability-token validation lives in `crate::auth` as an Axum extractor (`CapToken`).
//! This module is reserved for tower `Layer`s applied to the router (rate-limiting, etc. in M6).
