//! Errors, health, logs, and metrics for every TallyOwl service.
//!
//! `docs/CONVENTIONS.md` owns these four surfaces. They live here as shared code
//! rather than as a convention that each service reimplements, because the second
//! service otherwise builds to a different convention and both pay to converge.

pub mod error;
pub mod health;
pub mod http;
pub mod log;
pub mod metrics;
pub mod time;

pub use error::{ErrorCode, Result, TallyOwlError};
pub use health::{Cause, Check, CheckState, Health, HealthReport};
pub use log::{Logger, Severity};
pub use metrics::{MetricKind, Registry};
