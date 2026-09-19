//! Structured safe logging, from `docs/CONVENTIONS.md` section 4.
//!
//! Two lists govern a log line. The first says what every line carries: the
//! time, the severity, the service and its version, the workspace and project
//! when the work belongs to one, the request or batch ID when one exists, and
//! the message.
//!
//! The second says what a line never carries: a secret, a credential, a token or
//! a key; a request body, a header, or a cookie; personal data of any kind; and
//! an end-user ID at info level or below.
//!
//! The second list is enforced here rather than trusted to each call site. A
//! forbidden field is dropped and counted, so a mistake becomes visible in the
//! output instead of becoming a leak.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::time::{now_ms, to_utc_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Detail for an investigation.
    Debug,
    /// A state change that a person would want to know about.
    Info,
    /// Something failed and the system recovered.
    Warning,
    /// Something failed and a person must act.
    Error,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Debug => "debug",
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }

    pub fn parse(text: &str) -> Option<Severity> {
        Some(match text.to_lowercase().as_str() {
            "debug" => Severity::Debug,
            "info" => Severity::Info,
            "warning" | "warn" => Severity::Warning,
            "error" => Severity::Error,
            _ => return None,
        })
    }
}

/// Field names that never reach a log line at any level. A banned term
/// matches whole `_`/`-`/`.` separated segments, so `authorization_header`
/// and `x-api-key` are both caught while a word that merely contains a
/// banned fragment is not: the substring match this used to be censored
/// `files_reclaimed` — "claim" inside "reclaimed" — and an operational
/// counter silently vanished from the maintenance line for months (L170).
/// A plural segment matches its singular, so `claims` and `tokens` stay
/// refused.
const NEVER_LOGGED: &[&str] = &[
    "secret",
    "credential",
    "token",
    "password",
    "passphrase",
    "api_key",
    "apikey",
    "api-key",
    "private_key",
    "authorization",
    "cookie",
    "header",
    "body",
    "payload",
    "claim",
    "email",
    "phone",
    // A postal address is personal data. A network address is not, and a
    // service logs its own listen address on every start, so the rule names the
    // personal ones rather than the word they share.
    "postal_address",
    "street_address",
    "home_address",
    "billing_address",
    "ip_address",
    // Segment matching makes the bare word safe to ban: `client_ip` and
    // `ip_range` are caught, `description` and `shipment` are not.
    "ip",
];

/// Field names that carry an end-user identity. CONVENTIONS.md section 4 permits
/// these at debug level for a support investigation, and the project policy can
/// forbid even that.
const END_USER_FIELDS: &[&str] = &["end_user_id", "anonymous_id", "user_id"];

fn is_never_logged(key: &str) -> bool {
    let key = key.to_lowercase();
    let segments: Vec<&str> = key
        .split(['_', '-', '.'])
        .filter(|segment| !segment.is_empty())
        .collect();
    NEVER_LOGGED.iter().any(|banned| {
        let banned: Vec<&str> = banned.split(['_', '-']).collect();
        segments.windows(banned.len()).any(|window| {
            window
                .iter()
                .zip(banned.iter())
                .enumerate()
                .all(|(index, (have, want))| {
                    have == want
                        || (index + 1 == banned.len() && have.strip_suffix('s') == Some(want))
                })
        })
    })
}

fn is_end_user_field(key: &str) -> bool {
    let key = key.to_lowercase();
    END_USER_FIELDS.iter().any(|f| key == *f)
}

/// The context every line from this service carries.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub request_id: Option<String>,
    pub batch_id: Option<String>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn workspace(mut self, id: impl Into<String>) -> Self {
        self.workspace_id = Some(id.into());
        self
    }

    pub fn project(mut self, id: impl Into<String>) -> Self {
        self.project_id = Some(id.into());
        self
    }

    pub fn request(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }

    pub fn batch(mut self, id: impl Into<String>) -> Self {
        self.batch_id = Some(id.into());
        self
    }
}

/// Where a logger writes. Production writes to standard error; a test captures.
pub trait Sink: Send + Sync {
    fn write_line(&self, line: &str);
}

/// Standard error, one JSON object for each line.
pub struct StderrSink;

impl Sink for StderrSink {
    fn write_line(&self, line: &str) {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
    }
}

/// A sink that keeps lines in memory, so a test can assert what a service
/// logged and, more importantly, what it did not.
#[derive(Default)]
pub struct CaptureSink {
    lines: Mutex<Vec<String>>,
}

impl CaptureSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("capture lock").clone()
    }
}

impl Sink for CaptureSink {
    fn write_line(&self, line: &str) {
        self.lines
            .lock()
            .expect("capture lock")
            .push(line.to_string());
    }
}

/// A structured logger bound to one service and version.
pub struct Logger {
    service: String,
    version: String,
    level: Severity,
    sink: Arc<dyn Sink>,
    context: Context,
    /// How many fields this logger refused. A rising count is a defect in a call
    /// site, and it is visible rather than silent.
    refused_fields: Arc<AtomicU64>,
}

impl Logger {
    pub fn new(service: impl Into<String>, version: impl Into<String>, level: Severity) -> Self {
        Self {
            service: service.into(),
            version: version.into(),
            level,
            sink: Arc::new(StderrSink),
            context: Context::default(),
            refused_fields: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_sink(mut self, sink: Arc<dyn Sink>) -> Self {
        self.sink = sink;
        self
    }

    /// A child logger that adds context to every line. The refusal counter is
    /// shared, so one service reports one number.
    pub fn with_context(&self, context: Context) -> Logger {
        Logger {
            service: self.service.clone(),
            version: self.version.clone(),
            level: self.level,
            sink: Arc::clone(&self.sink),
            context,
            refused_fields: Arc::clone(&self.refused_fields),
        }
    }

    pub fn refused_field_count(&self) -> u64 {
        self.refused_fields.load(Ordering::Relaxed)
    }

    pub fn debug(&self, message: &str, fields: &[(&str, &str)]) {
        self.write(Severity::Debug, message, fields);
    }

    pub fn info(&self, message: &str, fields: &[(&str, &str)]) {
        self.write(Severity::Info, message, fields);
    }

    /// Something failed and the system recovered. An error that a retry fixed is
    /// a warning, because a log full of self-resolving errors trains people to
    /// ignore errors.
    pub fn warning(&self, message: &str, fields: &[(&str, &str)]) {
        self.write(Severity::Warning, message, fields);
    }

    pub fn error(&self, message: &str, fields: &[(&str, &str)]) {
        self.write(Severity::Error, message, fields);
    }

    fn write(&self, severity: Severity, message: &str, fields: &[(&str, &str)]) {
        if severity < self.level {
            return;
        }
        let ms = now_ms();
        let mut line = serde_json::Map::new();
        line.insert("time".into(), to_utc_text(ms).into());
        line.insert("time_ms".into(), ms.into());
        line.insert("severity".into(), severity.as_str().into());
        line.insert("service".into(), self.service.clone().into());
        line.insert("version".into(), self.version.clone().into());
        if let Some(v) = &self.context.workspace_id {
            line.insert("workspace_id".into(), v.clone().into());
        }
        if let Some(v) = &self.context.project_id {
            line.insert("project_id".into(), v.clone().into());
        }
        if let Some(v) = &self.context.request_id {
            line.insert("request_id".into(), v.clone().into());
        }
        if let Some(v) = &self.context.batch_id {
            line.insert("batch_id".into(), v.clone().into());
        }
        line.insert("message".into(), message.into());

        let mut refused = 0u64;
        for (key, value) in fields {
            if is_never_logged(key) {
                refused += 1;
                continue;
            }
            if is_end_user_field(key) && severity > Severity::Debug {
                refused += 1;
                continue;
            }
            line.insert((*key).to_string(), (*value).into());
        }
        if refused > 0 {
            self.refused_fields.fetch_add(refused, Ordering::Relaxed);
            line.insert("refused_fields".into(), refused.into());
        }
        self.sink
            .write_line(&serde_json::Value::Object(line).to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logger_with(level: Severity) -> (Logger, Arc<CaptureSink>) {
        let sink = CaptureSink::new();
        let logger = Logger::new("tallyowl-collector", "0.0.0", level)
            .with_sink(Arc::clone(&sink) as Arc<dyn Sink>);
        (logger, sink)
    }

    #[test]
    fn every_line_carries_the_required_fields() {
        let (logger, sink) = logger_with(Severity::Info);
        logger.info("Accepted a batch.", &[]);
        let line = &sink.lines()[0];
        for required in [
            "time", "time_ms", "severity", "service", "version", "message",
        ] {
            assert!(line.contains(required), "missing {required} in {line}");
        }
    }

    #[test]
    fn context_reaches_every_line() {
        let (logger, sink) = logger_with(Severity::Info);
        let scoped =
            logger.with_context(Context::new().workspace("w-1").project("p-1").batch("b-1"));
        scoped.info("Accepted a batch.", &[]);
        let line = &sink.lines()[0];
        assert!(line.contains("\"workspace_id\":\"w-1\""));
        assert!(line.contains("\"project_id\":\"p-1\""));
        assert!(line.contains("\"batch_id\":\"b-1\""));
    }

    #[test]
    fn a_forbidden_field_never_reaches_a_line() {
        let (logger, sink) = logger_with(Severity::Debug);
        logger.info(
            "Resolved a credential.",
            &[
                ("api_key", "tally_live_abc123"),
                ("authorization", "Bearer abc"),
                ("cookie", "session=1"),
                ("request_body", "{\"name\":\"x\"}"),
                ("source", "file"),
            ],
        );
        let line = &sink.lines()[0];
        assert!(!line.contains("tally_live_abc123"));
        assert!(!line.contains("Bearer"));
        assert!(!line.contains("session=1"));
        assert!(!line.contains("\"name\":\"x\""));
        assert!(
            line.contains("\"source\":\"file\""),
            "a safe field survives"
        );
        assert!(line.contains("\"refused_fields\":4"));
        assert_eq!(logger.refused_field_count(), 4);
    }

    #[test]
    fn a_banned_word_matches_whole_segments_not_substrings() {
        // Refused: the banned word stands as its own segment, plural, or a
        // multi-word banned term spans adjacent segments.
        for refused in [
            "claim",
            "claims",
            "claim_type",
            "x-api-key",
            "api_keys",
            "authorization_header",
            "client_ip",
            "ip_range",
            "session.token",
        ] {
            assert!(is_never_logged(refused), "{refused} must be refused");
        }
        // Passed: a banned fragment inside an ordinary word is not the word.
        // `files_reclaimed` is the field the substring match censored for
        // months (L170).
        for passed in [
            "files_reclaimed",
            "reclaimed",
            "shipment",
            "description",
            "recipient",
        ] {
            assert!(!is_never_logged(passed), "{passed} must pass");
        }
    }

    #[test]
    fn an_end_user_id_appears_at_debug_and_nowhere_else() {
        let (logger, sink) = logger_with(Severity::Debug);
        logger.debug("Investigating.", &[("end_user_id", "u-42")]);
        assert!(sink.lines()[0].contains("u-42"));

        logger.info("Ordinary work.", &[("end_user_id", "u-42")]);
        let info_line = &sink.lines()[1];
        assert!(!info_line.contains("u-42"));
        assert!(info_line.contains("\"refused_fields\":1"));
    }

    #[test]
    fn a_level_suppresses_a_quieter_line() {
        let (logger, sink) = logger_with(Severity::Warning);
        logger.debug("Detail.", &[]);
        logger.info("A state change.", &[]);
        assert!(sink.lines().is_empty());
        logger.warning("Recovered.", &[]);
        logger.error("A person must act.", &[]);
        assert_eq!(sink.lines().len(), 2);
    }

    #[test]
    fn a_short_field_name_does_not_catch_an_ordinary_word() {
        // "ip" must not refuse "pipeline" or "recipient_count".
        let (logger, sink) = logger_with(Severity::Info);
        logger.info("Working.", &[("pipeline", "delivery"), ("ip", "10.0.0.1")]);
        let line = &sink.lines()[0];
        assert!(line.contains("\"pipeline\":\"delivery\""));
        assert!(!line.contains("10.0.0.1"));
    }

    #[test]
    fn a_service_can_log_the_address_it_listens_on() {
        // A service logs this on every start, and it is not personal data. A
        // postal address is, and it is refused by its own name.
        let (logger, sink) = logger_with(Severity::Info);
        logger.info(
            "Accepting telemetry.",
            &[
                ("address", "127.0.0.1:5100"),
                ("postal_address", "1 Example Way"),
            ],
        );
        let line = &sink.lines()[0];
        assert!(line.contains("127.0.0.1:5100"), "{line}");
        assert!(!line.contains("Example Way"), "{line}");
        assert_eq!(logger.refused_field_count(), 1);
    }

    #[test]
    fn severity_parses_both_spellings_of_warning() {
        assert_eq!(Severity::parse("warn"), Some(Severity::Warning));
        assert_eq!(Severity::parse("WARNING"), Some(Severity::Warning));
        assert_eq!(Severity::parse("chatty"), None);
    }
}
