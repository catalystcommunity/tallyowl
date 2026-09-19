//! Collector intake and the forwarder role, as a library.
//!
//! The binary in `main.rs` wires these together from configuration. They live in
//! a library so that an integration test can run the same code against a real
//! head over a real socket, rather than against a second implementation.

pub mod compat;
/// The durable task boundary, which now lives in its own crate so that the
/// head can reach the same seam. See L137.
pub use tallyowl_queue as durable;
pub mod forwarder;
pub mod head_client;
pub mod intake;
pub mod policy;
pub mod selfobs;
pub mod series;
pub mod service;
pub mod task;
pub mod tenancy;

#[cfg(test)]
mod tests;
