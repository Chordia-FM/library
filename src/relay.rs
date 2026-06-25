//! DJ "Hybrid Relay" fallback (post-MVP). Internally:
//! - `client` - opens an authenticated, TLS-fingerprint-pinned HTTP Range pull to the DJ's
//!   library using the Hub-minted relay capability token.
//! - `proxy`  - buffers the pulled bytes and re-serves them to the local client over the same
//!   `client → own-library` path used for owned tracks.
//!
//! Backs `api::v1::relay`. The DJ's library serves at most one stream per relaying library.
