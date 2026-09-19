//! The head's own instruments, pushed as telemetry.
//!
//! D12 asks every service to be able to push the same instruments it exposes
//! into a protected internal project. The collector does it by handing items to
//! its own intake, because it has one. The head has none: it is the far side of
//! the durable queue.
//!
//! **So the head sends through a collector, using the app driver.** It is the
//! same path an application uses, which means the head's own metrics meet the
//! same acknowledgement rule as everything else: they reach the durable store
//! before anything reports success, and a collector that is down means the head
//! keeps serving and its metrics wait.
//!
//! Committing them straight into the store was the other option and it is the
//! wrong one. It would skip tenancy resolution, the series budget, and the
//! scrubber, so the one project an operator reads to find out whether TallyOwl
//! is healthy would be the one project that never went through TallyOwl's own
//! trust boundary.
//!
//! # The recursion guard
//!
//! `Registry::begin_publishing` suppresses recording for the length of one
//! push, so the storage work the push causes does not raise the counters the
//! push is reporting. See `tallyowl_obs::metrics` and D12.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_driver_rust::metrics::{Meter, Temporality};
use tallyowl_driver_rust::{Driver, Settings};
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{MetricKind, Registry};

/// The running publisher. Dropping the handle asks it to stop.
pub struct Publisher {
    stopping: Arc<AtomicBool>,
}

impl Publisher {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

/// Copy one registry reading into the driver's meter.
///
/// The meter is cumulative, and every registry reading is a running total since
/// the process started, so the two agree without conversion. A restarted head
/// reports a new `start_at`, which is what `rate` reads as a restart.
pub fn fill(meter: &Meter, registry: &Registry) {
    for reading in registry.snapshot() {
        let labels: tallyowl_driver_rust::metrics::Labels = reading.labels.clone();
        match reading.kind {
            MetricKind::Counter => {
                meter.counter(&reading.name, None, Some(&reading.help));
                // The registry holds the total and the meter accumulates, so
                // the difference is what to add. A first reading adds the whole
                // total, which is right: the meter starts at zero.
                let held = current(meter, &reading.name, &labels);
                let delta = reading.value - held;
                if delta > 0.0 {
                    let _ = meter.add(&reading.name, &labels, delta);
                }
            }
            MetricKind::Gauge => {
                meter.gauge(&reading.name, None, Some(&reading.help));
                let _ = meter.set(&reading.name, &labels, reading.value);
            }
            MetricKind::Histogram => {
                let Some(histogram) = reading.histogram else {
                    continue;
                };
                meter.histogram(&reading.name, &histogram.bounds, None, Some(&reading.help));
                // A registry histogram holds cumulative bucket counts and the
                // meter records observations. Replaying each observation would
                // be wrong twice over: it would cost one call for each sample
                // ever taken, and it would put every sample in the middle of
                // its bucket. `observe_bucket_counts` copies the shape instead.
                let held = held_histogram(meter, &reading.name, &labels);
                meter.observe_bucket_counts(
                    &reading.name,
                    &labels,
                    &histogram.counts,
                    histogram.count.saturating_sub(held.0),
                    histogram.sum - held.1,
                );
            }
        }
    }
}

fn current(meter: &Meter, name: &str, labels: &tallyowl_driver_rust::metrics::Labels) -> f64 {
    meter.value(name, labels).unwrap_or(0.0)
}

fn held_histogram(
    meter: &Meter,
    name: &str,
    labels: &tallyowl_driver_rust::metrics::Labels,
) -> (u64, f64) {
    meter.histogram_totals(name, labels).unwrap_or((0, 0.0))
}

/// Start pushing the head's instruments on a period.
///
/// Returns `None` when an operator did not ask for it, which is the default.
#[allow(clippy::too_many_arguments)]
pub fn start(
    enabled: bool,
    period: Duration,
    collector_address: &str,
    credential: &str,
    service_name: &str,
    registry: Arc<Registry>,
    logger: Arc<Logger>,
) -> Option<Publisher> {
    if !enabled {
        logger.info(
            "Self-observation is off, so this service publishes its instruments to its own endpoint and to nothing else. Turn it on with `metrics.selfObservation.enabled`.",
            &[],
        );
        return None;
    }
    if credential.is_empty() {
        // A push with no credential would be refused by intake on every period.
        // Saying so once beats a warning every minute.
        logger.warning(
            "Self-observation is on and no credential is configured, so this service cannot push its instruments. Set `collector.apiKey`.",
            &[],
        );
        return None;
    }

    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);
    let driver = Driver::new(Settings::new(collector_address, credential));
    let meter = Meter::with_temporality(Temporality::Cumulative).with_service(service_name);
    logger.info(
        "Publishing this service's own instruments to the internal project.",
        &[
            ("collector", collector_address),
            ("period_ms", &period.as_millis().to_string()),
        ],
    );

    std::thread::Builder::new()
        .name("tallyowl-self-observation".into())
        .spawn(move || {
            while !loop_stopping.load(Ordering::Relaxed) {
                let mut slept = Duration::ZERO;
                while slept < period && !loop_stopping.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    slept += Duration::from_millis(100);
                }
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                publish_once(&meter, &registry, &driver, &logger);
            }
        })
        .ok()?;
    Some(Publisher { stopping })
}

/// One push, with the recursion guard held for the whole of it.
pub fn publish_once(meter: &Meter, registry: &Registry, driver: &Driver, logger: &Logger) {
    {
        let Some(_guard) = registry.begin_publishing() else {
            // A push is already in flight. Two overlapping pushes would each
            // measure the other.
            return;
        };
        fill(meter, registry);
        if driver.publish_metrics(meter).is_err() {
            logger.warning(
                "This service could not buffer its own instruments. Its endpoint still serves them.",
                &[],
            );
            return;
        }
    }
    // The guard is released before the flush, because the flush reaches a
    // collector over a socket and holding a suppression flag across a network
    // call would hide real work for as long as that call took.
    if let Err(e) = driver.flush() {
        logger.warning(
            "This service could not publish its own instruments. Its endpoint still serves them.",
            &[("reason", &e.message)],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_driver_rust::metrics::labels as meter_labels;
    use tallyowl_obs::metrics::labels;

    fn registry() -> Arc<Registry> {
        let registry = Registry::new();
        registry
            .declare(
                "tallyowl_batches_committed_total",
                MetricKind::Counter,
                "Batches committed.",
                &[],
            )
            .unwrap();
        registry
            .declare(
                "tallyowl_segments_count",
                MetricKind::Gauge,
                "Sealed segments.",
                &[],
            )
            .unwrap();
        registry
            .declare(
                "tallyowl_commit_seconds",
                MetricKind::Histogram,
                "How long a commit took.",
                &[0.01, 0.1, 1.0],
            )
            .unwrap();
        registry
    }

    #[test]
    fn a_counter_reaches_the_meter_with_the_total_the_registry_holds() {
        let registry = registry();
        let meter = Meter::new();
        registry.add("tallyowl_batches_committed_total", &labels(&[]), 7);
        fill(&meter, &registry);
        let captures = meter.snapshot();
        let point = captures[0]
            .item()
            .metric_point
            .as_ref()
            .expect("a metric point");
        assert_eq!(point.metric_name, "tallyowl_batches_committed_total");
        assert_eq!(point.number_value, Some(7.0));
    }

    #[test]
    fn a_second_period_adds_only_what_happened_since_the_first() {
        // The registry holds a running total and the meter accumulates. Adding
        // the total again each period would double it.
        let registry = registry();
        let meter = Meter::new();
        registry.add("tallyowl_batches_committed_total", &labels(&[]), 7);
        fill(&meter, &registry);
        registry.add("tallyowl_batches_committed_total", &labels(&[]), 3);
        fill(&meter, &registry);

        let value = meter
            .value("tallyowl_batches_committed_total", &meter_labels(&[]))
            .expect("the meter holds it");
        assert_eq!(value, 10.0);
    }

    #[test]
    fn a_gauge_reaches_the_meter_at_its_current_level() {
        let registry = registry();
        let meter = Meter::new();
        registry.set_gauge("tallyowl_segments_count", &labels(&[]), 12);
        fill(&meter, &registry);
        registry.set_gauge("tallyowl_segments_count", &labels(&[]), 4);
        fill(&meter, &registry);
        assert_eq!(
            meter.value("tallyowl_segments_count", &meter_labels(&[])),
            Some(4.0)
        );
    }

    #[test]
    fn a_histogram_copies_its_shape_rather_than_replaying_every_observation() {
        let registry = registry();
        let meter = Meter::new();
        for value in [0.005, 0.05, 0.5] {
            registry.observe("tallyowl_commit_seconds", &labels(&[]), value);
        }
        fill(&meter, &registry);

        let captures = meter.snapshot();
        let histogram = captures
            .iter()
            .map(|c| c.item().metric_point.as_ref().expect("a point"))
            .find(|p| p.metric_name == "tallyowl_commit_seconds")
            .and_then(|p| p.histogram_value.clone())
            .expect("a histogram point");
        assert_eq!(histogram.bounds, vec![0.01, 0.1, 1.0]);
        assert_eq!(histogram.counts, vec![1, 2, 3]);
        assert_eq!(histogram.count, 3);
    }

    #[test]
    fn a_histogram_adds_only_the_new_observations_on_a_second_period() {
        let registry = registry();
        let meter = Meter::new();
        registry.observe("tallyowl_commit_seconds", &labels(&[]), 0.005);
        fill(&meter, &registry);
        registry.observe("tallyowl_commit_seconds", &labels(&[]), 0.005);
        fill(&meter, &registry);

        let (count, _) = meter
            .histogram_totals("tallyowl_commit_seconds", &meter_labels(&[]))
            .expect("the meter holds it");
        assert_eq!(count, 2, "two observations, not three");
    }

    #[test]
    fn self_observation_stays_off_unless_an_operator_asks() {
        let logger = Arc::new(Logger::new("test", "0", tallyowl_obs::log::Severity::Error));
        assert!(start(
            false,
            Duration::from_secs(60),
            "127.0.0.1:1",
            "key",
            "tallyowl-head",
            registry(),
            logger,
        )
        .is_none());
    }

    #[test]
    fn self_observation_with_no_credential_says_so_once_rather_than_failing_every_period() {
        let logger = Arc::new(Logger::new("test", "0", tallyowl_obs::log::Severity::Error));
        assert!(start(
            true,
            Duration::from_secs(60),
            "127.0.0.1:1",
            "",
            "tallyowl-head",
            registry(),
            logger,
        )
        .is_none());
    }
}
