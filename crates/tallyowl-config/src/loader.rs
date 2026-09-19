//! Resolve one configuration from every source, and say which source won.
//!
//! Precedence, from `docs/CONVENTIONS.md` section 5:
//!
//! 1. a command-line argument;
//! 2. an environment variable;
//! 3. a configuration file;
//! 4. the built-in default.
//!
//! The configuration file is YAML, and its tree is the same tree as the chart
//! values. A rendered chart and a local file are then the same document.
//!
//! **No service reads a `.env` file.** That convention belongs to a container
//! runtime, and it hides precedence. A container or compose workflow may supply
//! one, and the process still reads real environment variables.

use std::collections::BTreeMap;

use crate::schema::{environment_name, find, flag_name, Setting, SCHEMA};
use crate::value::{self, Kind, Value};

/// Where a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CommandLine,
    Environment,
    File,
    Default,
}

impl Source {
    /// What `config check` prints. A person reads this column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::CommandLine => "command line",
            Source::Environment => "environment",
            Source::File => "file",
            Source::Default => "default",
        }
    }
}

/// One setting, resolved.
#[derive(Debug, Clone)]
pub struct Entry {
    pub setting: &'static Setting,
    pub value: Value,
    pub source: Source,
}

/// A refusal. It names the setting, the value, and a valid example, because
/// CONVENTIONS.md section 5 requires all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub setting: String,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

fn refuse(setting: &Setting, value: &str, reason: &str) -> ConfigError {
    ConfigError {
        setting: setting.path.to_string(),
        message: format!(
            "The setting `{}` cannot take the value `{}`, because {}. A valid example is `{}`.",
            setting.path, value, reason, setting.example
        ),
    }
}

/// Every resolved setting, in key-path order.
#[derive(Debug, Clone)]
pub struct Resolved {
    entries: BTreeMap<String, Entry>,
    /// Keys that appeared in a source and match no setting. A typo in a
    /// configuration file otherwise does nothing at all, which is the worst
    /// possible outcome: the operator believes they changed something.
    pub unknown_keys: Vec<(String, Source)>,
}

impl Resolved {
    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn text(&self, path: &str) -> &str {
        self.entries
            .get(path)
            .and_then(|e| e.value.as_text())
            .unwrap_or("")
    }

    pub fn integer(&self, path: &str) -> i64 {
        self.entries
            .get(path)
            .and_then(|e| e.value.as_integer())
            .unwrap_or(0)
    }

    pub fn boolean(&self, path: &str) -> bool {
        self.entries
            .get(path)
            .and_then(|e| e.value.as_boolean())
            .unwrap_or(false)
    }

    pub fn list(&self, path: &str) -> Vec<String> {
        self.entries
            .get(path)
            .and_then(|e| e.value.as_list())
            .map(|l| l.to_vec())
            .unwrap_or_default()
    }

    pub fn source_of(&self, path: &str) -> Source {
        self.entries
            .get(path)
            .map(|e| e.source)
            .unwrap_or(Source::Default)
    }
}

/// The inputs a load reads. A test supplies its own, so a configuration test
/// changes no process environment and writes no file.
#[derive(Debug, Default, Clone)]
pub struct Inputs {
    /// The parsed configuration file, flattened to key paths.
    pub file: BTreeMap<String, String>,
    /// The process environment, in full. Only `TALLYOWL_` names are read.
    pub environment: BTreeMap<String, String>,
    /// Command-line arguments, as given.
    pub arguments: Vec<String>,
}

impl Inputs {
    pub fn from_host(file_text: Option<&str>) -> Result<Inputs, ConfigError> {
        let file = match file_text {
            Some(text) => flatten_yaml(text)?,
            None => BTreeMap::new(),
        };
        Ok(Inputs {
            file,
            environment: std::env::vars().collect(),
            arguments: std::env::args().skip(1).collect(),
        })
    }
}

/// Flatten a YAML document to key paths. The tree is the chart values tree, so
/// `storage: { receiptPolicy: local-one }` becomes `storage.receiptPolicy`.
pub fn flatten_yaml(text: &str) -> Result<BTreeMap<String, String>, ConfigError> {
    let root: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| ConfigError {
        setting: String::new(),
        message: format!("The configuration file is not valid YAML: {e}"),
    })?;
    let mut out = BTreeMap::new();
    flatten_into(&root, String::new(), &mut out);
    Ok(out)
}

fn flatten_into(node: &serde_yaml::Value, prefix: String, out: &mut BTreeMap<String, String>) {
    // An empty file parses to a single scalar with no key. It configures
    // nothing, which is a working home installation rather than a bad key.
    if prefix.is_empty() && !matches!(node, serde_yaml::Value::Mapping(_)) {
        return;
    }
    match node {
        serde_yaml::Value::Mapping(map) => {
            for (key, value) in map {
                let Some(key) = key.as_str() else { continue };
                let path = if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_into(value, path, out);
            }
        }
        serde_yaml::Value::Sequence(items) => {
            let joined: Vec<String> = items.iter().map(scalar_text).collect();
            out.insert(prefix, joined.join(","));
        }
        serde_yaml::Value::Null => {
            out.insert(prefix, String::new());
        }
        scalar => {
            out.insert(prefix, scalar_text(scalar));
        }
    }
}

fn scalar_text(node: &serde_yaml::Value) -> String {
    match node {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => String::new(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// Read `--key.path value` and `--key.path=value` from the argument list.
fn arguments_by_flag(arguments: &[String]) -> BTreeMap<String, String> {
    // One pass builds the flag form of every setting, so an argument resolves to
    // a key path by lookup rather than by guessing at the conversion backwards.
    let by_flag: BTreeMap<String, &'static str> =
        SCHEMA.iter().map(|s| (flag_name(s.path), s.path)).collect();

    let mut out = BTreeMap::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if !argument.starts_with("--") {
            index += 1;
            continue;
        }
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (argument.clone(), None),
        };
        let Some(path) = by_flag.get(&flag) else {
            index += 1;
            continue;
        };
        let value = match inline {
            Some(v) => {
                index += 1;
                v
            }
            None => {
                let next = arguments.get(index + 1).cloned().unwrap_or_default();
                index += 2;
                next
            }
        };
        out.insert((*path).to_string(), value);
    }
    out
}

/// Resolve every setting. This does not touch the file system or the process
/// environment; `Inputs` already holds both.
pub fn resolve(inputs: &Inputs) -> Result<Resolved, Vec<ConfigError>> {
    let arguments = arguments_by_flag(&inputs.arguments);
    let by_environment: BTreeMap<&'static str, String> = SCHEMA
        .iter()
        .filter_map(|s| {
            inputs
                .environment
                .get(&environment_name(s.path))
                .map(|v| (s.path, v.clone()))
        })
        .collect();

    let mut entries = BTreeMap::new();
    let mut errors = Vec::new();

    for setting in SCHEMA {
        let (text, source) = if let Some(v) = arguments.get(setting.path) {
            (v.clone(), Source::CommandLine)
        } else if let Some(v) = by_environment.get(setting.path) {
            (v.clone(), Source::Environment)
        } else if let Some(v) = inputs.file.get(setting.path) {
            (v.clone(), Source::File)
        } else {
            (setting.default.to_string(), Source::Default)
        };

        // A secret is a reference. Checking the shape here means a bad reference
        // stops startup at the same point as any other bad value.
        if setting.kind == Kind::Secret {
            if let Err(failure) = crate::secret::parse_reference(&text) {
                errors.push(refuse(setting, "the value given", &failure.reason));
                continue;
            }
        }

        match value::parse(&setting.kind, &text) {
            Ok(value) => {
                entries.insert(
                    setting.path.to_string(),
                    Entry {
                        setting,
                        value,
                        source,
                    },
                );
            }
            Err(failure) => errors.push(refuse(setting, &text, &failure.reason)),
        }
    }

    // A key that matches no setting is almost always a typo, and a typo that
    // does nothing is worse than one that stops startup.
    let mut unknown_keys = Vec::new();
    for key in inputs.file.keys() {
        if find(key).is_none() {
            unknown_keys.push((key.clone(), Source::File));
        }
    }
    for key in inputs.environment.keys() {
        if let Some(rest) = key.strip_prefix("TALLYOWL_") {
            if !rest.is_empty() && !SCHEMA.iter().any(|s| environment_name(s.path) == *key) {
                unknown_keys.push((key.clone(), Source::Environment));
            }
        }
    }

    if errors.is_empty() {
        Ok(Resolved {
            entries,
            unknown_keys,
        })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        Inputs::default()
    }

    #[test]
    fn an_empty_configuration_starts_a_working_home_installation() {
        let resolved = resolve(&inputs()).expect("the defaults are valid");
        assert_eq!(resolved.text("storage.receiptPolicy"), "local-one");
        assert_eq!(resolved.integer("storage.tabletVoters"), 1);
        assert_eq!(resolved.integer("corndogs.durableCopies"), 1);
        assert_eq!(resolved.text("integrity.mode"), "verify-on-read");
        assert!(!resolved.boolean("storage.coldTier.enabled"));
        for entry in resolved.entries() {
            assert_eq!(entry.source, Source::Default);
        }
    }

    #[test]
    fn the_file_tree_is_the_chart_values_tree() {
        let mut i = inputs();
        i.file = flatten_yaml(
            "storage:\n  receiptPolicy: local-quorum\n  tabletVoters: 3\ncollector:\n  roles:\n    - intake\n    - forwarder\n",
        )
        .unwrap();
        let resolved = resolve(&i).unwrap();
        assert_eq!(resolved.text("storage.receiptPolicy"), "local-quorum");
        assert_eq!(resolved.integer("storage.tabletVoters"), 3);
        assert_eq!(resolved.source_of("storage.tabletVoters"), Source::File);
        assert_eq!(
            resolved.list("collector.roles"),
            vec!["intake", "forwarder"]
        );
    }

    #[test]
    fn precedence_runs_command_line_then_environment_then_file_then_default() {
        let mut i = inputs();
        i.file = flatten_yaml("log:\n  level: debug\n").unwrap();
        let resolved = resolve(&i).unwrap();
        assert_eq!(resolved.text("log.level"), "debug");
        assert_eq!(resolved.source_of("log.level"), Source::File);

        i.environment
            .insert("TALLYOWL_LOG__LEVEL".into(), "warning".into());
        let resolved = resolve(&i).unwrap();
        assert_eq!(resolved.text("log.level"), "warning");
        assert_eq!(resolved.source_of("log.level"), Source::Environment);

        i.arguments = vec!["--log.level".into(), "error".into()];
        let resolved = resolve(&i).unwrap();
        assert_eq!(resolved.text("log.level"), "error");
        assert_eq!(resolved.source_of("log.level"), Source::CommandLine);
    }

    #[test]
    fn a_flag_accepts_both_the_joined_and_the_separated_form() {
        let mut i = inputs();
        i.arguments = vec!["--log.level=error".into()];
        assert_eq!(resolve(&i).unwrap().text("log.level"), "error");

        i.arguments = vec!["--storage.receipt-policy".into(), "local-quorum".into()];
        assert_eq!(
            resolve(&i).unwrap().text("storage.receiptPolicy"),
            "local-quorum"
        );
    }

    #[test]
    fn a_bad_value_names_the_setting_the_value_and_an_example() {
        let mut i = inputs();
        i.file = flatten_yaml("integrity:\n  mode: paranoid\n").unwrap();
        let errors = resolve(&i).unwrap_err();
        assert_eq!(errors.len(), 1);
        let text = &errors[0].message;
        assert!(text.contains("integrity.mode"), "{text}");
        assert!(text.contains("paranoid"), "{text}");
        assert!(text.contains("verify-on-read"), "{text}");
    }

    #[test]
    fn every_bad_value_is_reported_rather_than_only_the_first() {
        // A person who fixes one setting and restarts, then fixes the next and
        // restarts, pays for every round trip. Report all of them at once.
        let mut i = inputs();
        i.file = flatten_yaml(
            "integrity:\n  mode: paranoid\nlog:\n  level: chatty\ncell:\n  controllers: many\n",
        )
        .unwrap();
        let errors = resolve(&i).unwrap_err();
        assert_eq!(errors.len(), 3);
    }

    #[test]
    fn a_secret_must_be_a_reference() {
        let mut i = inputs();
        i.file = flatten_yaml("collector:\n  apiKey: tally_live_abc123\n").unwrap();
        let errors = resolve(&i).unwrap_err();
        assert!(errors[0].message.contains("collector.apiKey"));
        assert!(errors[0].message.contains("never a value"));
        // The refusal must not repeat the secret into a log.
        assert!(!errors[0].message.contains("tally_live_abc123"));

        i.file = flatten_yaml("collector:\n  apiKey: file:/etc/tallyowl/key\n").unwrap();
        assert!(resolve(&i).is_ok());
    }

    #[test]
    fn a_key_that_matches_no_setting_is_reported() {
        let mut i = inputs();
        i.file = flatten_yaml("storage:\n  reciptPolicy: local-one\n").unwrap();
        i.environment
            .insert("TALLYOWL_STORAGE__RECIPT_POLICY".into(), "local-one".into());
        let resolved = resolve(&i).unwrap();
        assert_eq!(resolved.unknown_keys.len(), 2);
        assert!(resolved
            .unknown_keys
            .iter()
            .any(|(k, s)| k == "storage.reciptPolicy" && *s == Source::File));
        assert!(resolved
            .unknown_keys
            .iter()
            .any(|(_, s)| *s == Source::Environment));
    }

    #[test]
    fn an_environment_variable_from_another_product_is_ignored() {
        let mut i = inputs();
        i.environment.insert("PATH".into(), "/usr/bin".into());
        i.environment
            .insert("CORNDOGS_LISTEN".into(), "0.0.0.0:5080".into());
        let resolved = resolve(&i).unwrap();
        assert!(resolved.unknown_keys.is_empty());
    }

    #[test]
    fn an_empty_file_configures_nothing_rather_than_one_bad_key() {
        for text in ["", "\n", "# only a comment\n", "---\n"] {
            assert!(
                flatten_yaml(text).unwrap().is_empty(),
                "`{text:?}` should configure nothing"
            );
        }
    }

    #[test]
    fn a_configuration_file_that_is_not_yaml_says_so() {
        let error = flatten_yaml("storage: [unclosed\n").unwrap_err();
        assert!(error.message.contains("not valid YAML"));
    }

    #[test]
    fn a_flag_this_service_does_not_know_is_left_alone() {
        // A binary takes its own arguments as well as settings. An unknown flag
        // is not a configuration error here.
        let mut i = inputs();
        i.arguments = vec!["config".into(), "check".into(), "--verbose".into()];
        assert!(resolve(&i).is_ok());
    }
}
