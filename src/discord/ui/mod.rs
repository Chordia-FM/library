//! Everything a listener sees: the Components V2 builder, the shared vocabulary, the views, and
//! the code that delivers them.
//!
//! The design sheet is `views.rs`; read it as the spec for how the bot looks. `v2.rs` is the only
//! place that knows Discord's JSON, `fmt.rs` is the only place that knows how a duration or a badge
//! is written, and `send.rs` is the only place that talks to the HTTP API.

pub mod custom_id;
pub mod fmt;
pub mod send;
pub mod v2;
pub mod views;

pub use v2::Message;
