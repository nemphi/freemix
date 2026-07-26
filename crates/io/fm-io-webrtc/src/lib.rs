//! Transport-neutral WebRTC signaling, media, and data-channel contracts.
//!
//! This crate intentionally contains no browser or native WebRTC adapter. The
//! [`fake`] module supplies a deterministic in-memory implementation for tests
//! and simulations.

mod contract;
pub mod fake;

pub use contract::*;

#[cfg(test)]
mod tests;
