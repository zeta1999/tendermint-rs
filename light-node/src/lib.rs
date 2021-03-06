//! LightNode
//!
//! The Tendermint light-node wraps the light-client crate into a command-line interface tool.
//! It can be used as a standalone light client daemon and exposes a JSONRPC endpoint from which
//! you can query the current state of the light node.

// Tip: Deny warnings with `RUSTFLAGS="-D warnings"` environment variable in CI

#![forbid(unsafe_code)]
#![warn(
    rust_2018_idioms,
    trivial_casts,
    unused_lifetimes,
    unused_qualifications
)]

pub mod application;
pub mod commands;
pub mod config;
pub mod error;
pub mod prelude;
pub mod requester;
pub mod rpc;
