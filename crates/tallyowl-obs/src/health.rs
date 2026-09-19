//! Live and ready, from `docs/CONVENTIONS.md` section 3.
//!
//! The two states answer two different questions. Live asks whether the process
//! is working or whether something should restart it. Ready asks whether the
//! process can do its job right now.
//!
//! **Readiness fails when the service cannot safely do its job.** A service that
//! cannot reach its durable store fails readiness. It never accepts data that it
//! would then discard.
//!
//! A component can be alive, ready, and unwell. A degraded state therefore names
//! the most specific cause the component can *establish*, never the most likely
//! one it can guess. `Unknown` is a valid cause and carries evidence instead of a
//! guess, because a wrong cause sends a person to look at the wrong thing. See
//! D60.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Why a component is degraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A cause the component measured and can defend.
    Established(String),
    /// The component knows it is unwell and cannot establish why. The evidence
    /// is the measurements that led to the state, each against the baseline the
    /// component compared it to.
    Unknown { evidence: Vec<(String, String)> },
}

impl Cause {
    pub fn summary(&self) -> String {
        match self {
            Cause::Established(text) => text.clone(),
            Cause::Unknown { .. } => {
                "Slow or unwell for a reason we could not establish. The measurements are below."
                    .to_string()
            }
        }
    }
}

/// The state of one named check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckState {
    /// The check passes.
    Ok,
    /// The check fails. `reason` is written for a person, in the language of
    /// CONVENTIONS.md section 1: "Cannot reach the durable store", never
    /// "corndogs_conn=nil".
    Failed { reason: String },
    /// The check passes and the component is unwell.
    Degraded { cause: Cause },
}

impl CheckState {
    pub fn is_ready(&self) -> bool {
        !matches!(self, CheckState::Failed { .. })
    }
}

/// One named check and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
}

/// The snapshot a health endpoint returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    pub live: bool,
    pub ready: bool,
    pub checks: Vec<Check>,
}

impl HealthReport {
    /// The plain-language summary a person reads first.
    pub fn summary(&self) -> String {
        if !self.live {
            return "This service is not working and should be restarted.".to_string();
        }
        let failed: Vec<&Check> = self.checks.iter().filter(|c| !c.state.is_ready()).collect();
        if failed.is_empty() {
            let degraded: Vec<&Check> = self
                .checks
                .iter()
                .filter(|c| matches!(c.state, CheckState::Degraded { .. }))
                .collect();
            if degraded.is_empty() {
                return "This service is working normally.".to_string();
            }
            let causes: Vec<String> = degraded
                .iter()
                .map(|c| match &c.state {
                    CheckState::Degraded { cause } => cause.summary(),
                    _ => unreachable!(),
                })
                .collect();
            return format!(
                "This service is working and is not healthy. {}",
                causes.join(" ")
            );
        }
        let reasons: Vec<String> = failed
            .iter()
            .map(|c| match &c.state {
                CheckState::Failed { reason } => reason.clone(),
                _ => unreachable!(),
            })
            .collect();
        format!(
            "This service cannot do its job right now. {}",
            reasons.join(" ")
        )
    }

    pub fn to_json(&self) -> String {
        let checks: Vec<serde_json::Value> = self
            .checks
            .iter()
            .map(|c| {
                let mut entry = serde_json::Map::new();
                entry.insert("name".into(), c.name.clone().into());
                match &c.state {
                    CheckState::Ok => {
                        entry.insert("state".into(), "ok".into());
                    }
                    CheckState::Failed { reason } => {
                        entry.insert("state".into(), "failed".into());
                        entry.insert("reason".into(), reason.clone().into());
                    }
                    CheckState::Degraded { cause } => {
                        entry.insert("state".into(), "degraded".into());
                        match cause {
                            Cause::Established(text) => {
                                entry.insert("cause".into(), text.clone().into());
                            }
                            Cause::Unknown { evidence } => {
                                entry.insert("cause".into(), "unknown".into());
                                let ev: serde_json::Map<String, serde_json::Value> = evidence
                                    .iter()
                                    .map(|(k, v)| (k.clone(), serde_json::Value::from(v.clone())))
                                    .collect();
                                entry.insert("evidence".into(), ev.into());
                            }
                        }
                    }
                }
                entry.into()
            })
            .collect();
        serde_json::json!({
            "live": self.live,
            "ready": self.ready,
            "summary": self.summary(),
            "checks": checks,
        })
        .to_string()
    }
}

/// The health of one service. Every check is named, and readiness is the
/// conjunction of every check.
///
/// A check starts `Failed`. A service is therefore not ready until it has proved
/// each of its dependencies, rather than ready until something proves otherwise.
#[derive(Debug, Default)]
pub struct Health {
    live: Mutex<bool>,
    checks: Mutex<BTreeMap<String, CheckState>>,
}

impl Health {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            live: Mutex::new(true),
            checks: Mutex::new(BTreeMap::new()),
        })
    }

    /// Declare a check. It starts failed with the reason a person sees until the
    /// service proves it.
    pub fn declare(&self, name: &str, reason_until_proved: &str) {
        self.checks.lock().expect("health lock").insert(
            name.to_string(),
            CheckState::Failed {
                reason: reason_until_proved.to_string(),
            },
        );
    }

    pub fn set(&self, name: &str, state: CheckState) {
        self.checks
            .lock()
            .expect("health lock")
            .insert(name.to_string(), state);
    }

    pub fn pass(&self, name: &str) {
        self.set(name, CheckState::Ok);
    }

    pub fn fail(&self, name: &str, reason: impl Into<String>) {
        self.set(
            name,
            CheckState::Failed {
                reason: reason.into(),
            },
        );
    }

    /// Mark the process not working. Something should restart it.
    pub fn stop_living(&self) {
        *self.live.lock().expect("health lock") = false;
    }

    pub fn report(&self) -> HealthReport {
        let checks = self.checks.lock().expect("health lock");
        let list: Vec<Check> = checks
            .iter()
            .map(|(name, state)| Check {
                name: name.clone(),
                state: state.clone(),
            })
            .collect();
        let ready = list.iter().all(|c| c.state.is_ready());
        HealthReport {
            live: *self.live.lock().expect("health lock"),
            ready,
            checks: list,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.report().ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_check_starts_failed() {
        let health = Health::new();
        health.declare("durable-store", "Cannot reach the durable store.");
        assert!(!health.is_ready());
        let report = health.report();
        assert!(report.live);
        assert!(report.summary().contains("durable store"));
    }

    #[test]
    fn readiness_needs_every_check() {
        let health = Health::new();
        health.declare("durable-store", "Cannot reach the durable store.");
        health.declare("timeout-sweep", "The retry sweep has not run yet.");
        health.pass("durable-store");
        assert!(!health.is_ready(), "one passing check is not readiness");
        health.pass("timeout-sweep");
        assert!(health.is_ready());
    }

    #[test]
    fn a_failed_check_names_the_cause_in_plain_language() {
        let health = Health::new();
        health.declare("durable-store", "unused");
        health.fail("durable-store", "Cannot reach the durable store.");
        let summary = health.report().summary();
        assert!(summary.contains("Cannot reach the durable store."));
        // The rule that made this a decision: no identifier leaks into the text.
        assert!(!summary.contains("corndogs_conn"));
    }

    #[test]
    fn a_degraded_check_stays_ready() {
        let health = Health::new();
        health.declare("storage", "unused");
        health.set(
            "storage",
            CheckState::Degraded {
                cause: Cause::Established("The disk is answering slowly.".into()),
            },
        );
        assert!(health.is_ready(), "degraded is not the same as not ready");
        assert!(health.report().summary().contains("answering slowly"));
    }

    #[test]
    fn an_unknown_cause_reports_its_evidence_rather_than_a_guess() {
        // D60. A component that guesses a cause it cannot establish sends an
        // operator down a wrong path, which is worse than sending them nowhere.
        let health = Health::new();
        health.declare("storage", "unused");
        health.set(
            "storage",
            CheckState::Degraded {
                cause: Cause::Unknown {
                    evidence: vec![
                        (
                            "append_latency_ms".into(),
                            "94 against a median of 8".into(),
                        ),
                        ("fsync_latency_ms".into(), "5 against a median of 5".into()),
                    ],
                },
            },
        );
        let json = health.report().to_json();
        assert!(json.contains("\"cause\":\"unknown\""));
        assert!(json.contains("append_latency_ms"));
        assert!(json.contains("median"));
    }

    #[test]
    fn a_dead_process_reports_that_it_should_restart() {
        let health = Health::new();
        health.stop_living();
        let report = health.report();
        assert!(!report.live);
        assert!(report.summary().contains("restarted"));
    }
}
