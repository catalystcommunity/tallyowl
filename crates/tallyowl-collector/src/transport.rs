//! How the collector's listeners and its calls to the head are secured. D62.
//!
//! The rule is one sentence: a connection that crosses a network uses TLS. A
//! loopback or `unix:` address crosses none, so it stays plaintext, and the
//! home profile needs no certificate at all.
//!
//! Intake and the OpenTelemetry receiver are the listeners applications reach.
//! They present a certificate from `tls.certificateDirectories`, which the
//! operator supplies, and an application proves itself with its project key
//! above TLS. The collector reads those files again on `tls.reloadInterval`, so
//! a certificate replaced before it expires needs no restart.
//!
//! The collector's calls to the head are mutual TLS with an enrolled identity.
//! That identity comes from `tallyowl-identity`. Until the collector holds
//! one, a call to the head fails as retryable, and the forwarder's breaker
//! waits; intake is not affected, because it needs only its own certificate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, MetricKind, Registry};
use tallyowl_rpc::material::{CertificateSet, Reload, ReloadDriver, Reloadable};
use tallyowl_rpc::{Address, Dispatcher, Server, ServerOptions};

/// Seconds until the last certificate a listener holds expires.
pub const EXPIRY_GAUGE: &str = "tallyowl_tls_certificate_expiry_seconds";
/// Certificate files that changed and could not be used.
pub const RELOAD_FAILURES: &str = "tallyowl_tls_reload_failures_total";
/// Connections a listener refused during the TLS handshake.
pub const HANDSHAKES_REFUSED: &str = "tallyowl_tls_handshakes_refused_total";

/// The listeners that present an application certificate. The label on the two
/// instruments above takes only these values.
pub const LISTENER_INTAKE: &str = "intake";
pub const LISTENER_OTLP: &str = "otlp";

/// How one listener is exposed, as the startup log says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// A loopback or `unix:` address. Nothing crosses a network.
    Local,
    /// TLS with the application certificate.
    Tls,
    /// A network address in plaintext, because `transport.allowPlaintext` says so.
    Plaintext,
}

impl Exposure {
    /// The words the startup log uses.
    pub fn describe(self) -> &'static str {
        match self {
            Exposure::Local => "plaintext, because the address is loopback or a unix socket",
            Exposure::Tls => "TLS, with the certificates in `tls.certificateDirectories`",
            Exposure::Plaintext => {
                "plaintext on a network address, because `transport.allowPlaintext` is true"
            }
        }
    }
}

/// Decide how an application-facing listener is exposed.
///
/// `config check` refuses the configuration this refuses, so a service that
/// started never reaches the refusal. It is here too, because a process that
/// binds a network address in plaintext by mistake is the defect D62 exists
/// to stop.
pub fn exposure(
    setting: &str,
    address: &str,
    allow_plaintext: bool,
    has_certificates: bool,
) -> Result<Exposure, TallyOwlError> {
    let parsed = Address::parse(address)?;
    if parsed.plaintext_permitted() {
        return Ok(Exposure::Local);
    }
    if has_certificates {
        return Ok(Exposure::Tls);
    }
    if allow_plaintext {
        return Ok(Exposure::Plaintext);
    }
    Err(TallyOwlError::invalid_argument(format!(
        "`{setting}` is `{address}`, which applications reach over a network, and `tls.certificateDirectories` is empty. Set `tls.certificateDirectories`, listen on a loopback or `unix:` address, or set `transport.allowPlaintext: true` if something else protects this network."
    )))
}

/// Load the application certificates, or `None` when no directory is named.
pub fn application_certificates(
    directories: &[String],
    now_ms: i64,
) -> Result<Option<Arc<CertificateSet>>, TallyOwlError> {
    if directories.is_empty() {
        return Ok(None);
    }
    let paths: Vec<_> = directories.iter().map(std::path::PathBuf::from).collect();
    CertificateSet::load(&paths, now_ms).map(|set| Some(Arc::new(set)))
}

/// Serve intake by the rule: plaintext on a local address, TLS otherwise.
pub fn serve_intake(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    options: ServerOptions,
    certificates: Option<Arc<CertificateSet>>,
    allow_plaintext: bool,
) -> Result<(Server, Exposure), TallyOwlError> {
    let exposed = exposure(
        "collector.listen",
        address,
        allow_plaintext,
        certificates.is_some(),
    )?;
    let server = match (exposed, certificates) {
        (Exposure::Tls, Some(set)) => {
            tallyowl_rpc::tls::serve_server_auth(address, dispatcher, options, set)?
        }
        _ => tallyowl_rpc::serve_with(address, dispatcher, options).map_err(|e| {
            TallyOwlError::unavailable(format!(
                "Intake could not listen on `{address}` (`collector.listen`). {e}"
            ))
        })?,
    };
    Ok((server, exposed))
}

/// The TLS configuration the OpenTelemetry receiver serves, or `None` when it
/// serves plaintext by the same rule as intake.
pub fn receiver_tls(
    address: &str,
    certificates: Option<&Arc<CertificateSet>>,
    allow_plaintext: bool,
) -> Result<(Option<Arc<rustls::ServerConfig>>, Exposure), TallyOwlError> {
    let exposed = exposure(
        "compatibility.openTelemetry.listen",
        address,
        allow_plaintext,
        certificates.is_some(),
    )?;
    let config = match (exposed, certificates) {
        (Exposure::Tls, Some(set)) => Some(tallyowl_rpc::tls::server_auth_config(Arc::clone(set))),
        _ => None,
    };
    Ok((config, exposed))
}

/// Declare the TLS instruments. A registry accepts a name once.
pub fn declare_metrics(metrics: &Registry) {
    for (name, kind, help) in [
        (
            EXPIRY_GAUGE,
            MetricKind::Gauge,
            "Seconds until the last application certificate a listener holds expires. Replace it before this reaches zero.",
        ),
        (
            RELOAD_FAILURES,
            MetricKind::Counter,
            "Certificate files that changed and could not be used. The listener kept the certificate it had.",
        ),
        (
            HANDSHAKES_REFUSED,
            MetricKind::Counter,
            "Connections refused during the TLS handshake: a plaintext client, or a certificate no trusted authority signed. A steady rise is a misconfigured client or a stranger.",
        ),
    ] {
        metrics.declare(name, kind, help, &[]).unwrap_or_else(|e| {
            panic!("the metric `{name}` is not a name the registry accepts: {}", e.0)
        });
    }
}

/// Reads the application certificates again on an interval, and keeps the
/// expiry gauge current. [`CertificateWatch::tick`] takes the time, so a test
/// moves the clock rather than waiting.
pub struct CertificateWatch {
    set: Arc<CertificateSet>,
    listeners: Vec<&'static str>,
    driver: ReloadDriver,
    metrics: Arc<Registry>,
    logger: Arc<Logger>,
}

impl CertificateWatch {
    /// One set of certificates, served by the named listeners.
    pub fn new(
        set: Arc<CertificateSet>,
        listeners: Vec<&'static str>,
        interval: Duration,
        metrics: Arc<Registry>,
        logger: Arc<Logger>,
    ) -> CertificateWatch {
        let driver = ReloadDriver::new(vec![Arc::clone(&set) as Arc<dyn Reloadable>], interval);
        CertificateWatch {
            set,
            listeners,
            driver,
            metrics,
            logger,
        }
    }

    /// Publish the expiry, and reload when the interval has passed.
    pub fn tick(&self, now_ms: i64) {
        if let Some(results) = self.driver.tick(now_ms) {
            for result in results {
                match result {
                    Reload::Unchanged => {}
                    Reload::Replaced => self.logger.info(
                        "The application certificate changed, and the next connection uses the new one.",
                        &[(
                            "directory",
                            &self.set.current_directory().display().to_string(),
                        )],
                    ),
                    Reload::Kept(reason) => {
                        for listener in &self.listeners {
                            self.metrics
                                .increment(RELOAD_FAILURES, &labels(&[("listener", listener)]));
                        }
                        self.logger.warning(
                            "A certificate file changed and could not be used. The listener keeps the certificate it had.",
                            &[("reason", &reason)],
                        );
                    }
                }
            }
        }
        let left = self.set.seconds_left(now_ms);
        for listener in &self.listeners {
            self.metrics
                .set_gauge(EXPIRY_GAUGE, &labels(&[("listener", listener)]), left);
        }
    }

    /// Run [`CertificateWatch::tick`] on a thread until `stop` is set.
    pub fn spawn(self, stop: Arc<AtomicBool>) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("tallyowl-tls-watch".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    self.tick(tallyowl_rpc::material::now_ms());
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
    }
}

/// The warning `transport.allowPlaintext` earns, naming what it exposes. `None`
/// when nothing is exposed.
pub fn plaintext_warning(exposed: &[(&str, &str, Exposure)]) -> Option<String> {
    let named: Vec<String> = exposed
        .iter()
        .filter(|(_, _, how)| *how == Exposure::Plaintext)
        .map(|(setting, address, _)| format!("`{setting}` ({address})"))
        .collect();
    if named.is_empty() {
        return None;
    }
    Some(format!(
        "`transport.allowPlaintext` is true, so these listeners serve plaintext on a network address, and a project key crosses that network in the clear: {}. Protect the network, or give the listeners certificates.",
        named.join(", ")
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tallyowl_obs::log::{CaptureSink, Severity};

    pub(crate) const HOUR_MS: i64 = 3_600_000;

    /// A certificate for `127.0.0.1` from a throwaway authority, valid from
    /// `from_ms` for `life_ms`, written as `tls.crt` and `tls.key` in `dir`.
    pub(crate) fn write_pair(dir: &std::path::Path, from_ms: i64, life_ms: i64) -> Vec<u8> {
        use rcgen::{CertificateParams, IsCa, KeyPair, SanType};
        let when = |ms: i64| time::OffsetDateTime::from_unix_timestamp(ms / 1000).expect("time");
        let ca_key = KeyPair::generate().expect("key");
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("params");
        ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.not_before = when(from_ms - HOUR_MS);
        ca.not_after = when(from_ms + 100 * HOUR_MS);
        let ca = ca.self_signed(&ca_key).expect("authority");

        let key = KeyPair::generate().expect("key");
        let mut leaf = CertificateParams::new(Vec::<String>::new()).expect("params");
        leaf.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().expect("ip"))];
        leaf.not_before = when(from_ms);
        leaf.not_after = when(from_ms + life_ms);
        let leaf = leaf.signed_by(&key, &ca, &ca_key).expect("leaf");

        std::fs::create_dir_all(dir).expect("dir");
        std::fs::write(dir.join("tls.crt"), leaf.pem()).expect("write");
        std::fs::write(dir.join("tls.key"), key.serialize_pem()).expect("write");
        ca.der().to_vec()
    }

    pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tallyowl-transport-{name}-{}-{}",
            std::process::id(),
            tallyowl_rpc::material::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_local_address_is_plaintext_and_a_network_address_needs_a_certificate() {
        assert_eq!(
            exposure("collector.listen", "127.0.0.1:5100", false, false).unwrap(),
            Exposure::Local
        );
        assert_eq!(
            exposure("collector.listen", "unix:/run/intake.sock", false, false).unwrap(),
            Exposure::Local
        );
        assert_eq!(
            exposure("collector.listen", "0.0.0.0:5100", false, true).unwrap(),
            Exposure::Tls
        );
        assert_eq!(
            exposure("collector.listen", "0.0.0.0:5100", true, false).unwrap(),
            Exposure::Plaintext
        );
        let refused = exposure("collector.listen", "0.0.0.0:5100", false, false).unwrap_err();
        assert!(
            refused.message.contains("tls.certificateDirectories"),
            "{}",
            refused.message
        );
        assert!(
            refused.message.contains("collector.listen"),
            "{}",
            refused.message
        );
    }

    #[test]
    fn a_certificate_would_rather_be_used_than_plaintext_even_when_plaintext_is_allowed() {
        assert_eq!(
            exposure("collector.listen", "0.0.0.0:5100", true, true).unwrap(),
            Exposure::Tls
        );
    }

    #[test]
    fn the_warning_names_each_exposed_listener_and_only_those() {
        assert!(
            plaintext_warning(&[("collector.listen", "127.0.0.1:5100", Exposure::Local)]).is_none()
        );
        let warning = plaintext_warning(&[
            ("collector.listen", "0.0.0.0:5100", Exposure::Plaintext),
            ("head.listen", "127.0.0.1:5110", Exposure::Local),
        ])
        .expect("a warning");
        assert!(
            warning.contains("`collector.listen` (0.0.0.0:5100)"),
            "{warning}"
        );
        assert!(!warning.contains("head.listen"), "{warning}");
    }

    #[test]
    fn a_replaced_certificate_is_picked_up_and_a_broken_one_is_counted() {
        let now = tallyowl_rpc::material::now_ms();
        let dir = scratch("rotate");
        write_pair(&dir, now - HOUR_MS, 2 * HOUR_MS);
        let set = application_certificates(&[dir.display().to_string()], now)
            .expect("loads")
            .expect("a set");
        let metrics = Registry::new();
        declare_metrics(&metrics);
        let sink = CaptureSink::new();
        let logger =
            Arc::new(Logger::new("collector", "test", Severity::Info).with_sink(sink.clone()));
        let watch = CertificateWatch::new(
            Arc::clone(&set),
            vec![LISTENER_INTAKE, LISTENER_OTLP],
            Duration::from_secs(30),
            Arc::clone(&metrics),
            logger,
        );

        watch.tick(now);
        let intake = labels(&[("listener", LISTENER_INTAKE)]);
        let first = metrics.gauge_value(EXPIRY_GAUGE, &intake);
        assert!(first > 0 && first <= 3600, "the gauge reads {first}");

        // A new certificate that lives longer. Nothing reads it before the
        // interval passes.
        write_pair(&dir, now - HOUR_MS, 48 * HOUR_MS);
        watch.tick(now + 10_000);
        assert_eq!(metrics.gauge_value(EXPIRY_GAUGE, &intake), first - 10);
        watch.tick(now + 31_000);
        let replaced = metrics.gauge_value(EXPIRY_GAUGE, &intake);
        assert!(
            replaced > 40 * 3600,
            "the new certificate was not picked up: {replaced}"
        );
        assert!(sink
            .lines()
            .iter()
            .any(|l| l.contains("certificate changed")));

        // A key that no longer parses is refused, counted for each listener,
        // and the listener keeps what it had.
        std::fs::write(dir.join("tls.key"), "not a key").expect("write");
        watch.tick(now + 62_000);
        assert_eq!(metrics.counter_value(RELOAD_FAILURES, &intake), 1);
        assert_eq!(
            metrics.counter_value(RELOAD_FAILURES, &labels(&[("listener", LISTENER_OTLP)])),
            1
        );
        assert!(metrics.gauge_value(EXPIRY_GAUGE, &intake) > 40 * 3600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_directory_means_no_certificates_and_a_missing_directory_is_refused() {
        assert!(application_certificates(&[], 0).expect("empty").is_none());
        assert!(application_certificates(&["/nonexistent/tallyowl".to_string()], 0).is_err());
    }
}
