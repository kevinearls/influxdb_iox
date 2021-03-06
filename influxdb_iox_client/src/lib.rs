//! An InfluxDB IOx API client.
#![deny(
    broken_intra_doc_links,
    rust_2018_idioms,
    missing_debug_implementations,
    unreachable_pub
)]
#![warn(
    missing_docs,
    clippy::todo,
    clippy::dbg_macro,
    clippy::clone_on_ref_ptr
)]
#![allow(clippy::missing_docs_in_private_items)]

pub use generated_types::{protobuf_type_url, protobuf_type_url_eq};

pub use client::*;

/// Builder for constructing connections for use with the various gRPC clients
pub mod connection;

#[cfg(feature = "format")]
/// Output formatting utilities
pub mod format;

mod client;
