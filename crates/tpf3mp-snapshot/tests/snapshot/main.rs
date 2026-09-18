//! Integration tests: chunking, manifests from hostile peers, the chunk store
//! and transfers between stores.

// `allow-unwrap-in-tests` covers `#[test]` functions only, not the helpers here.
#![allow(clippy::unwrap_used)]

mod chunking;
mod common;
mod manifest;
mod store;
mod transfer;
