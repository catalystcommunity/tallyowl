//! Head-side tail sampling, from D35.
//!
//! Tail sampling needs a complete trace before it decides. A span routes by its
//! trace ID, so one tablet already holds every span of one trace, and no
//! component buffers a trace across collectors.
//!
//! The sequence D35 gives, and what each step is here:
//!
//! | Step | Here |
//! | --- | --- |
//! | The collector forwards every span, head sampling only | `tallyowl-collector` |
//! | The head commits the spans to the tablet that owns the trace | `Ingest::commit` |
//! | Those spans enter a provisional retention class | [`OpenTraces`] holds them |
//! | The projector applies the tail rules when the window closes | [`TailSampler::sweep`] |
//! | A kept trace moves to its normal retention class | a durable `keep` decision |
//! | A dropped trace gets a tombstone; compaction reclaims | a durable tombstone |
//!
//! # Two states, and only one of them needs a record
//!
//! A **dropped** trace gets a tombstone. A tombstone is a standing predicate,
//! so it also hides the late spans of that trace that arrive afterwards. That
//! is exactly the behaviour a dropped trace needs, and it comes for free.
//!
//! A **kept** trace needs no predicate, because nothing hides it. It still gets
//! a durable record, for one reason: a span that arrives after the grace period
//! must not change a decision that was already applied. Without the record, a
//! late span could make a kept trace look droppable at the next sweep.
//!
//! # Always keep
//!
//! D35: "An unhandled error or a critical business event must survive even when
//! the tail rules later drop its trace." The tombstone therefore excludes the
//! kinds that must survive, and the exclusion is part of the predicate rather
//! than a list of event IDs, because a predicate also covers a late arrival
//! that no list could have named.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;
use tallyowl_store::catalog::Tombstone;
use tallyowl_store::row::{hex, EventRow, PropertyValue};
use tallyowl_store::{SegmentedStore, Store};

/// The kinds a tail decision never drops.
///
/// An error is the one a person goes looking for after an incident, and a
/// conversion is money. Dropping either because a sampler thought the trace was
/// ordinary would be the wrong kind of correct.
pub const ALWAYS_KEEP_KINDS: &[&str] = &["error", "conversion"];

/// What the sampler was configured with.
#[derive(Debug, Clone, Copy)]
pub struct TailSettings {
    /// How long after a trace's last span the decision is made. DECISIONS.md
    /// gives 60 seconds.
    pub decision_window_ms: i64,
    /// How long after the decision a late span is still expected. DECISIONS.md
    /// gives 30 seconds. A span later than this cannot change an applied
    /// decision; it is counted and the existing decision applies.
    pub late_span_grace_ms: i64,
    /// The share of ordinary traces to keep, from 0 to 100.
    pub keep_percent: f64,
    /// A trace at least this long is always kept. A slow trace is the one
    /// somebody is looking for, and sampling it away wastes the whole feature.
    pub keep_slower_than_ms: i64,
}

impl Default for TailSettings {
    fn default() -> TailSettings {
        TailSettings {
            decision_window_ms: 60_000,
            late_span_grace_ms: 30_000,
            keep_percent: 100.0,
            keep_slower_than_ms: 2_000,
        }
    }
}

/// What the sampler decided, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub keep: bool,
    /// The rule that decided, in the words a person reads in an explain output.
    pub reason: &'static str,
}

/// Traces that have not been decided yet.
///
/// Held in memory, because a decision window is a minute and the whole point is
/// that it is short. A restart loses the registry, and the sweep after a
/// restart rebuilds it from the rows that are still provisional. Nothing is
/// lost by that: an undecided trace is a kept trace until somebody decides
/// otherwise.
#[derive(Debug, Default)]
pub struct OpenTraces {
    inner: Mutex<BTreeMap<[u8; 16], Open>>,
}

#[derive(Debug, Clone)]
struct Open {
    project_id: [u8; 16],
    first_at: i64,
    last_at: i64,
}

impl OpenTraces {
    pub fn new() -> Arc<OpenTraces> {
        Arc::new(OpenTraces::default())
    }

    /// Note every trace one commit touched.
    pub fn observe(&self, rows: &[EventRow]) {
        let mut inner = self.inner.lock().expect("trace registry");
        for row in rows {
            let Some(trace_id) = row.trace_id else {
                continue;
            };
            let at = row.received_at;
            inner
                .entry(trace_id)
                .and_modify(|open| {
                    open.last_at = open.last_at.max(at);
                    open.first_at = open.first_at.min(at);
                })
                .or_insert(Open {
                    project_id: row.project_id,
                    first_at: at,
                    last_at: at,
                });
        }
    }

    /// Traces whose decision window has closed.
    fn closed(&self, now: i64, window_ms: i64) -> Vec<([u8; 16], Open)> {
        let inner = self.inner.lock().expect("trace registry");
        inner
            .iter()
            .filter(|(_, open)| now - open.last_at >= window_ms)
            .map(|(id, open)| (*id, open.clone()))
            .collect()
    }

    fn forget(&self, trace_id: &[u8; 16]) {
        self.inner.lock().expect("trace registry").remove(trace_id);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("trace registry").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The projector that applies the tail rules.
pub struct TailSampler {
    pub store: Arc<SegmentedStore>,
    /// Where an erasure goes. It is the tablet in a replicated installation and
    /// the local store at home, and it is separate from `store` because an
    /// erasure has to reach every replica while a tail decision is this
    /// replica's own record. See L098.
    pub erasing: Arc<dyn Store>,
    pub open: Arc<OpenTraces>,
    pub settings: TailSettings,
    pub metrics: Arc<Registry>,
    pub logger: Arc<Logger>,
    pub stopping: AtomicBool,
    /// Late spans that arrived after the grace period of an applied decision.
    /// An operator needs this number: a rising count means the decision window
    /// is shorter than the traces this installation actually produces.
    pub late_after_grace: AtomicU64,
}

impl TailSampler {
    pub fn declare_metrics(metrics: &Registry) {
        metrics.declare(
            "tallyowl_tail_decisions_total",
            tallyowl_obs::MetricKind::Counter,
            "Tail-sampling decisions, by outcome.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_tail_decisions_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_tail_open_traces_count",
            tallyowl_obs::MetricKind::Gauge,
            "Traces waiting for their decision window to close.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_tail_open_traces_count` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_tail_late_spans_total",
            tallyowl_obs::MetricKind::Counter,
            "Spans that arrived after the grace period of an applied decision.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_tail_late_spans_total` is not a name the registry accepts: {}", e.0));
    }

    /// One pass. Public so a test can run exactly one rather than race a thread.
    pub fn sweep(&self, now: i64) -> usize {
        let mut decided = 0;
        for (trace_id, open) in self.open.closed(now, self.settings.decision_window_ms) {
            match self.decide_one(trace_id, &open, now) {
                Ok(true) => decided += 1,
                Ok(false) => {}
                Err(e) => {
                    // A decision that could not be recorded is not applied. The
                    // trace stays open and the next sweep tries again, which is
                    // right: a trace nobody decided is a trace nobody dropped.
                    self.logger.warning(
                        "A trace could not be decided and will be decided again.",
                        &[("trace_id", &hex(&trace_id)), ("reason", &e)],
                    );
                    continue;
                }
            }
            self.open.forget(&trace_id);
        }
        self.metrics.set_gauge(
            "tallyowl_tail_open_traces_count",
            &labels(&[]),
            self.open.len() as i64,
        );
        decided
    }

    fn decide_one(&self, trace_id: [u8; 16], open: &Open, now: i64) -> Result<bool, String> {
        // A trace that was already decided is not decided again. A late span
        // cannot change an applied decision, which is the rule D35 states.
        if let Some(applied) = self
            .store
            .catalog()
            .tail_decision(trace_id)
            .map_err(|e| e.to_string())?
        {
            if now - applied.1 > self.settings.late_span_grace_ms {
                self.late_after_grace.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .increment("tallyowl_tail_late_spans_total", &labels(&[]));
            }
            return Ok(false);
        }

        let found = self
            .store
            .lookup_correlated(tallyowl_store::segment::schema::TRACE_ID, &trace_id)
            .map_err(|e| e.to_string())?;
        if found.rows.is_empty() {
            // Every row of this trace is already gone, by erasure or retention.
            // Nothing to decide.
            return Ok(true);
        }

        let decision = self.judge(&found.rows, trace_id);
        self.metrics.increment(
            "tallyowl_tail_decisions_total",
            &labels(&[("outcome", if decision.keep { "keep" } else { "drop" })]),
        );

        if decision.keep {
            self.store
                .catalog()
                .record_tail_decision(trace_id, true, now)
                .map_err(|e| e.to_string())?;
            return Ok(true);
        }

        // A dropped trace gets a tombstone. It hides the spans now and it keeps
        // hiding the late ones, which is why it is a predicate rather than a
        // list of the event IDs in hand.
        let tombstone = Tombstone {
            tombstone_id: tombstone_id(trace_id),
            generation: 0,
            project_id: open.project_id,
            event_ids: Vec::new(),
            property: Some(("trace_id".to_string(), hex(&trace_id))),
            except_kinds: ALWAYS_KEEP_KINDS.iter().map(|k| k.to_string()).collect(),
            range: None,
            requested_at: now,
            // The predicate stays active for late arrivals. A span of a dropped
            // trace that arrives an hour later must not become visible.
            horizon: now + self.settings.decision_window_ms * 60,
            reason: format!("tail sampling: {}", decision.reason),
        };
        self.erasing.erase(&tombstone).map_err(|e| e.to_string())?;
        self.store
            .catalog()
            .record_tail_decision(trace_id, false, now)
            .map_err(|e| e.to_string())?;
        Ok(true)
    }

    /// Apply the tail rules to one complete trace.
    ///
    /// The rules run in order and the first that applies decides, so a reader
    /// can say why a trace was kept without simulating the whole set.
    pub fn judge(&self, rows: &[EventRow], trace_id: [u8; 16]) -> Decision {
        // An error or a conversion anywhere in the trace keeps the whole trace.
        // Keeping the error and dropping its spans would leave a person with a
        // failure and no way to see what led to it.
        if rows
            .iter()
            .any(|row| ALWAYS_KEEP_KINDS.contains(&row.kind.as_str()))
        {
            return Decision {
                keep: true,
                reason: "the trace holds an error or a conversion",
            };
        }
        if rows.iter().any(|row| {
            matches!(
                row.properties.get("status"),
                Some((PropertyValue::Text(status), _)) if status == "error"
            )
        }) {
            return Decision {
                keep: true,
                reason: "a span in the trace failed",
            };
        }
        if self.span_of(rows) >= self.settings.keep_slower_than_ms {
            return Decision {
                keep: true,
                reason: "the trace was slow",
            };
        }
        if self.settings.keep_percent >= 100.0 {
            return Decision {
                keep: true,
                reason: "every ordinary trace is kept",
            };
        }
        // The trace ID decides, not a random source. The same trace decides the
        // same way on every node and after every restart, which is what makes a
        // decision reproducible and a sampled result explainable.
        let share = u64::from_be_bytes(trace_id[..8].try_into().expect("eight bytes"));
        let threshold =
            ((self.settings.keep_percent.clamp(0.0, 100.0) / 100.0) * u64::MAX as f64) as u64;
        if share < threshold {
            Decision {
                keep: true,
                reason: "the trace is in the sampled share",
            }
        } else {
            Decision {
                keep: false,
                reason: "an ordinary trace outside the sampled share",
            }
        }
    }

    /// How long the trace took, from its earliest start to its latest end.
    fn span_of(&self, rows: &[EventRow]) -> i64 {
        let mut earliest = i64::MAX;
        let mut latest = i64::MIN;
        for row in rows {
            let start = row.occurred_at;
            let duration = match row.properties.get("duration_ms") {
                Some((PropertyValue::Integer(ms), _)) => *ms,
                Some((PropertyValue::Unsigned(ms), _)) => *ms as i64,
                _ => 0,
            };
            earliest = earliest.min(start);
            latest = latest.max(start + duration);
        }
        if earliest == i64::MAX {
            0
        } else {
            latest - earliest
        }
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

/// A stable identifier for the tombstone of one trace, so a repeated decision
/// writes one record rather than one for each attempt.
fn tombstone_id(trace_id: [u8; 16]) -> [u8; 16] {
    let hash = blake3::hash(&[b"tail-sampling".as_slice(), &trace_id].concat());
    hash.as_bytes()[..16].try_into().expect("sixteen bytes")
}

/// Run the sampler on an interval.
pub fn run(sampler: Arc<TailSampler>, interval: Duration) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tallyowl-tail".into())
        .spawn(move || {
            while !sampler.stopping.load(Ordering::Relaxed) {
                sampler.sweep(now_ms());
                std::thread::sleep(interval);
            }
        })
        .expect("the tail sampling thread starts")
}
