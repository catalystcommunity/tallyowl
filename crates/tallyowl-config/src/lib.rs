//! The configuration loader every TallyOwl binary uses.
//!
//! `docs/PLAN.md` Phase 1 puts this first, and says why: configuration is the
//! part of that phase that prevents the most later work. One name for one
//! setting, in the file, the environment, the flag, and the Helm value; a
//! secret that is a reference rather than a value; a refusal at startup rather
//! than a surprise in production; and `config check` on the first day.

pub mod check;
pub mod loader;
pub mod schema;
pub mod secret;
pub mod validate;
pub mod value;

pub use loader::{ConfigError, Entry, Inputs, Resolved, Source};
pub use schema::{environment_name, flag_name, Setting, SCHEMA};
pub use secret::{Secret, SecretRef};
pub use value::{Kind, Value};

/// A validated configuration. A `Config` exists only when every setting parsed
/// and every cross-setting rule held, so nothing downstream re-checks a value.
#[derive(Debug, Clone)]
pub struct Config {
    resolved: Resolved,
    /// Where the file came from, for the `config check` header. `None` when no
    /// file existed, which is still a working home installation.
    file_path: Option<String>,
}

impl Config {
    /// Resolve and validate from an in-memory set of inputs. Every test uses
    /// this; nothing in it touches the file system or the process environment.
    pub fn from_inputs(inputs: &Inputs) -> Result<Config, Vec<ConfigError>> {
        let resolved = loader::resolve(inputs)?;
        validate::check_rules(&resolved)?;
        Ok(Config {
            resolved,
            file_path: None,
        })
    }

    /// Resolve and validate from the real host: the named file when it exists,
    /// the process environment, and the command line.
    ///
    /// A missing file is not an error. Every value has a home-safe default, so
    /// an installation with no configuration file starts a working system.
    pub fn load_from_host(file_path: &str) -> Result<Config, Vec<ConfigError>> {
        let exists = std::path::Path::new(file_path).exists();
        let text = if exists {
            match std::fs::read_to_string(file_path) {
                Ok(text) => Some(text),
                Err(e) => {
                    return Err(vec![ConfigError {
                        setting: String::new(),
                        message: format!(
                            "The configuration file {file_path} could not be read: {e}"
                        ),
                    }])
                }
            }
        } else {
            None
        };
        let inputs = Inputs::from_host(text.as_deref()).map_err(|e| vec![e])?;
        let mut config = Config::from_inputs(&inputs)?;
        config.file_path = exists.then(|| file_path.to_string());
        Ok(config)
    }

    pub fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    pub fn text(&self, path: &str) -> &str {
        self.resolved.text(path)
    }

    pub fn integer(&self, path: &str) -> i64 {
        self.resolved.integer(path)
    }

    pub fn boolean(&self, path: &str) -> bool {
        self.resolved.boolean(path)
    }

    pub fn list(&self, path: &str) -> Vec<String> {
        self.resolved.list(path)
    }

    /// A duration setting, in the milliseconds CONVENTIONS.md section 7 requires.
    pub fn duration_ms(&self, path: &str) -> i64 {
        self.resolved.integer(path)
    }

    pub fn bytes(&self, path: &str) -> i64 {
        self.resolved.integer(path)
    }

    /// Resolve a secret reference to its value. The reference is in the
    /// configuration; the value never is.
    ///
    /// The caller logs that the secret resolved and from where. It never logs
    /// the value.
    pub fn secret(&self, path: &str) -> Result<Secret, ConfigError> {
        let text = self.resolved.text(path);
        let reference = secret::parse_reference(text).map_err(|f| ConfigError {
            setting: path.to_string(),
            message: format!(
                "The setting `{path}` is not a valid secret reference: {}",
                f.reason
            ),
        })?;
        secret::resolve(&reference, &secret::HostSecretSource).map_err(|f| ConfigError {
            setting: path.to_string(),
            message: format!(
                "The secret for `{path}` could not be resolved: {}",
                f.reason
            ),
        })
    }

    /// Whether this process runs a named collector role.
    pub fn has_role(&self, role: &str) -> bool {
        self.list("collector.roles").iter().any(|r| r == role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_installation_needs_no_configuration_file() {
        let config = Config::from_inputs(&Inputs::default()).expect("the defaults are valid");
        assert_eq!(config.text("installation.profile"), "home");
        assert_eq!(config.text("storage.receiptPolicy"), "local-one");
        assert!(config.has_role("intake"));
        assert!(config.has_role("forwarder"));
        assert!(!config.has_role("compatibility-receiver"));
    }

    #[test]
    fn a_duration_reaches_the_caller_in_milliseconds() {
        let config = Config::from_inputs(&Inputs::default()).unwrap();
        assert_eq!(config.duration_ms("corndogs.sweepInterval"), 1_000);
        assert_eq!(config.duration_ms("integrity.scrub.period"), 604_800_000);
    }

    #[test]
    fn a_byte_count_reaches_the_caller_in_bytes() {
        let config = Config::from_inputs(&Inputs::default()).unwrap();
        assert_eq!(config.bytes("collector.maxBatchBytes"), 524_288);
        assert_eq!(config.bytes("corndogs.maxPayloadBytes"), 16_777_216);
    }
}
