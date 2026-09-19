//! A secret is a reference, never a value.
//!
//! The configuration file holds `file:` or `env:`. The loader resolves the
//! reference at startup, logs that it resolved and from where, and never logs
//! the value. A literal value in a configuration file is refused, because a
//! configuration file gets copied, pasted, and committed.

use std::fmt;

/// A reference to a secret. The reference is safe to print; the value is not,
/// and this type never holds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// Read the secret from a file. The whole file is the secret, less
    /// surrounding whitespace.
    File(String),
    /// Read the secret from an environment variable.
    Environment(String),
    /// The setting is not configured. Every secret is optional in the home
    /// profile, so an empty configuration file starts a working system.
    Absent,
}

/// A resolved secret. `Debug` and `Display` show the reference, never the value,
/// so a secret cannot reach a log through an accidental format.
#[derive(Clone)]
pub struct Secret {
    value: String,
    from: SecretRef,
}

impl Secret {
    /// The value. Every call site that uses this is a place to check for a leak.
    pub fn expose(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Where the value came from, for the line that says a secret resolved.
    pub fn source_description(&self) -> String {
        describe(&self.from)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(from {})", describe(&self.from))
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[a secret]")
    }
}

fn describe(reference: &SecretRef) -> String {
    match reference {
        SecretRef::File(path) => format!("the file {path}"),
        SecretRef::Environment(name) => format!("the environment variable {name}"),
        SecretRef::Absent => "nowhere; it is not configured".to_string(),
    }
}

/// Why a secret reference was refused. The text reaches a person, so it says
/// what to do rather than what went wrong internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFailure {
    pub reason: String,
}

/// Read a reference. This does not touch the file system.
pub fn parse_reference(text: &str) -> Result<SecretRef, SecretFailure> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(SecretRef::Absent);
    }
    if let Some(path) = text.strip_prefix("file:") {
        if path.trim().is_empty() {
            return Err(SecretFailure {
                reason: "`file:` names no file. Write `file:/etc/tallyowl/collector.key`."
                    .to_string(),
            });
        }
        return Ok(SecretRef::File(path.trim().to_string()));
    }
    if let Some(name) = text.strip_prefix("env:") {
        if name.trim().is_empty() {
            return Err(SecretFailure {
                reason: "`env:` names no environment variable. Write `env:TALLYOWL_COLLECTOR_KEY`."
                    .to_string(),
            });
        }
        return Ok(SecretRef::Environment(name.trim().to_string()));
    }
    Err(SecretFailure {
        reason: "a secret must be a reference, never a value. Write `file:/path/to/the/secret` or `env:NAME`."
            .to_string(),
    })
}

/// What the resolver reads from. A test supplies its own, so a secret test needs
/// no file on disk and no environment change.
pub trait SecretSource {
    fn read_file(&self, path: &str) -> Result<String, String>;
    fn read_environment(&self, name: &str) -> Result<String, String>;
}

/// The real file system and process environment.
pub struct HostSecretSource;

impl SecretSource for HostSecretSource {
    fn read_file(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }

    fn read_environment(&self, name: &str) -> Result<String, String> {
        std::env::var(name).map_err(|_| "it is not set".to_string())
    }
}

/// Resolve a reference to a value.
pub fn resolve(reference: &SecretRef, source: &dyn SecretSource) -> Result<Secret, SecretFailure> {
    let value = match reference {
        SecretRef::Absent => String::new(),
        SecretRef::File(path) => source.read_file(path).map_err(|e| SecretFailure {
            // The reason names the file, never its content.
            reason: format!("the file {path} could not be read: {e}"),
        })?,
        SecretRef::Environment(name) => {
            source.read_environment(name).map_err(|e| SecretFailure {
                reason: format!("the environment variable {name} could not be read: {e}"),
            })?
        }
    };
    Ok(Secret {
        value: value.trim().to_string(),
        from: reference.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeSource {
        files: BTreeMap<String, String>,
        environment: BTreeMap<String, String>,
    }

    impl SecretSource for FakeSource {
        fn read_file(&self, path: &str) -> Result<String, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| "there is no such file".to_string())
        }
        fn read_environment(&self, name: &str) -> Result<String, String> {
            self.environment
                .get(name)
                .cloned()
                .ok_or_else(|| "it is not set".to_string())
        }
    }

    #[test]
    fn a_literal_value_is_refused_and_the_message_says_what_to_write() {
        let failure = parse_reference("tally_live_abc123").unwrap_err();
        assert!(failure.reason.contains("never a value"));
        assert!(failure.reason.contains("file:"));
        assert!(failure.reason.contains("env:"));
        // The refusal must not repeat the value back into a log.
        assert!(!failure.reason.contains("tally_live_abc123"));
    }

    #[test]
    fn a_file_reference_resolves_and_trims() {
        let mut source = FakeSource::default();
        source
            .files
            .insert("/etc/tallyowl/collector.key".into(), "abc123\n".into());
        let reference = parse_reference("file:/etc/tallyowl/collector.key").unwrap();
        let secret = resolve(&reference, &source).unwrap();
        assert_eq!(secret.expose(), "abc123");
        assert!(secret
            .source_description()
            .contains("/etc/tallyowl/collector.key"));
    }

    #[test]
    fn an_environment_reference_resolves() {
        let mut source = FakeSource::default();
        source
            .environment
            .insert("TALLYOWL_COLLECTOR_KEY".into(), "xyz".into());
        let reference = parse_reference("env:TALLYOWL_COLLECTOR_KEY").unwrap();
        assert_eq!(resolve(&reference, &source).unwrap().expose(), "xyz");
    }

    #[test]
    fn a_missing_source_names_the_reference_and_not_the_value() {
        let source = FakeSource::default();
        let reference = parse_reference("file:/nowhere").unwrap();
        let failure = resolve(&reference, &source).unwrap_err();
        assert!(failure.reason.contains("/nowhere"));
        assert!(failure.reason.contains("could not be read"));
    }

    #[test]
    fn an_absent_secret_resolves_to_nothing_rather_than_failing() {
        // Every value has a default that is safe for a home installation, so an
        // empty configuration file starts a working system.
        let source = FakeSource::default();
        let secret = resolve(&parse_reference("").unwrap(), &source).unwrap();
        assert!(secret.is_empty());
    }

    #[test]
    fn a_secret_never_formats_as_its_value() {
        let mut source = FakeSource::default();
        source.files.insert("/k".into(), "super-secret".into());
        let secret = resolve(&parse_reference("file:/k").unwrap(), &source).unwrap();
        assert!(!format!("{secret}").contains("super-secret"));
        assert!(!format!("{secret:?}").contains("super-secret"));
        assert_eq!(format!("{secret}"), "[a secret]");
    }

    #[test]
    fn an_empty_reference_target_is_refused() {
        assert!(parse_reference("file:").is_err());
        assert!(parse_reference("env:  ").is_err());
    }
}
