//! The head, as a library.
//!
//! The binary in `main.rs` wires these together from configuration. They live in
//! a library so that an integration test can commit through the real ingest path
//! and query through the real query path.

pub mod admin;
pub mod alerts;
pub mod analysis;
pub mod attribution;
pub mod calendar;
pub mod campaign;
pub mod cluster;
pub mod control;
pub mod dashboard;
pub mod enrollment;
pub mod erasure;
pub mod errors;
pub mod expr;
pub mod identity;
pub mod ingest;
pub mod linkkeys;
pub mod money;
pub mod notify;
pub mod passes;
pub mod policy;
pub mod project;
pub mod query;
pub mod rollup;
pub mod sampling;
pub mod saved;
pub mod selfobs;
pub mod service;
pub mod starter;
pub mod wire;
pub mod workflows;

/// Declare every instrument this process reports, in one place.
///
/// A gauge that is never declared is silently dropped when it is set, so a
/// declaration the binary forgets produces a dashboard panel that says "no
/// data" for ever. That happened once: the storage instruments existed and were
/// sampled, and nothing declared them outside their own tests, so a running
/// head reported none of them. One function, called once, and a test that reads
/// the exposition rather than the source.
pub fn declare_metrics(metrics: &tallyowl_obs::metrics::Registry) {
    ingest::Ingest::declare_metrics(metrics);
    control::ControlService::declare_metrics(metrics);
    sampling::TailSampler::declare_metrics(metrics);
    linkkeys::SignIn::declare_metrics(metrics);
    tallyowl_store::metrics::declare(metrics);
}

#[cfg(test)]
mod tests;
