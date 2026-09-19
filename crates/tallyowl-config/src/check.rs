//! `tallyowl config check`.
//!
//! It resolves the full configuration from every source, reports which source
//! won for each value, masks every secret, and exits non-zero on an invalid
//! value. `docs/PLAN.md` Phase 1 says this one command removes more debugging
//! time than any other in that phase, and it exists on the first day for that
//! reason.

use crate::loader::{self, ConfigError, Inputs};
use crate::validate;
use crate::value::Kind;

/// What `config check` produced.
pub struct Report {
    pub text: String,
    /// The exit code the binary returns. Non-zero on an invalid value.
    pub exit_code: i32,
}

/// Run the check over a set of inputs and render the report.
pub fn run(inputs: &Inputs, file_path: Option<&str>) -> Report {
    let mut text = String::new();

    match file_path {
        Some(path) if std::path::Path::new(path).exists() => {
            text.push_str(&format!("Configuration file: {path}\n"));
        }
        Some(path) => {
            text.push_str(&format!(
                "Configuration file: {path} does not exist. Every value below is a default or comes from the environment or the command line.\n"
            ));
        }
        None => {
            text.push_str("Configuration file: none. Every value below is a default or comes from the environment or the command line.\n");
        }
    }
    text.push('\n');

    let resolved = match loader::resolve(inputs) {
        Ok(resolved) => resolved,
        Err(errors) => {
            text.push_str(&render_errors(&errors));
            return Report { text, exit_code: 1 };
        }
    };

    let width = resolved
        .entries()
        .map(|e| e.setting.path.len())
        .max()
        .unwrap_or(0);
    let value_width = resolved
        .entries()
        .map(|e| display_value(e).len())
        .max()
        .unwrap_or(0)
        .max("value".len());

    text.push_str(&format!(
        "{:<width$}  {:<value_width$}  {}\n",
        "setting", "value", "source"
    ));
    text.push_str(&format!(
        "{}  {}  {}\n",
        "-".repeat(width),
        "-".repeat(value_width),
        "-".repeat(12)
    ));
    for entry in resolved.entries() {
        text.push_str(&format!(
            "{:<width$}  {:<value_width$}  {}\n",
            entry.setting.path,
            display_value(entry),
            entry.source.as_str()
        ));
    }

    let mut exit_code = 0;

    if !resolved.unknown_keys.is_empty() {
        text.push_str("\nThese keys match no setting. Check them for a spelling mistake, because a key that matches no setting changes nothing:\n");
        for (key, source) in &resolved.unknown_keys {
            text.push_str(&format!("  {key}  (from the {})\n", source.as_str()));
        }
        // A key that does nothing is a defect in the deployment, and a check
        // that passes anyway lets it survive.
        exit_code = 1;
    }

    if let Err(errors) = validate::check_rules(&resolved) {
        text.push('\n');
        text.push_str(&render_errors(&errors));
        exit_code = 1;
    }

    if exit_code == 0 {
        text.push_str("\nThis configuration is valid.\n");
    }

    Report { text, exit_code }
}

fn render_errors(errors: &[ConfigError]) -> String {
    let mut text = String::new();
    if errors.len() == 1 {
        text.push_str("This configuration cannot start the service. One setting is wrong:\n\n");
    } else {
        text.push_str(&format!(
            "This configuration cannot start the service. {} settings are wrong:\n\n",
            errors.len()
        ));
    }
    for error in errors {
        text.push_str(&format!("  {}\n", error.message));
    }
    text
}

/// What one entry shows in the report. A secret shows its reference and never
/// its value, because a check that prints a secret is worse than no check.
fn display_value(entry: &crate::loader::Entry) -> String {
    if entry.setting.kind == Kind::Secret {
        let reference = entry.value.to_display();
        if reference.is_empty() {
            return "[not configured]".to_string();
        }
        return format!("[a secret, from {reference}]");
    }
    let shown = entry.value.to_display();
    if shown.is_empty() {
        "[empty]".to_string()
    } else {
        shown
    }
}

/// The whole command, against the real host.
pub fn run_on_host(file_path: &str) -> Report {
    let text = std::fs::read_to_string(file_path).ok();
    match Inputs::from_host(text.as_deref()) {
        Ok(inputs) => run(&inputs, Some(file_path)),
        Err(error) => Report {
            text: render_errors(&[error]),
            exit_code: 1,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::flatten_yaml;

    fn check(yaml: &str) -> Report {
        let inputs = Inputs {
            file: flatten_yaml(yaml).expect("valid YAML"),
            ..Inputs::default()
        };
        run(&inputs, None)
    }

    #[test]
    fn a_valid_configuration_exits_zero_and_says_so() {
        let report = check("");
        assert_eq!(report.exit_code, 0);
        assert!(report.text.contains("This configuration is valid."));
    }

    #[test]
    fn the_report_names_the_winning_source_for_every_value() {
        let mut inputs = Inputs {
            file: flatten_yaml("log:\n  level: debug\n").unwrap(),
            arguments: vec!["--storage.tablet-voters=1".into()],
            ..Inputs::default()
        };
        inputs
            .environment
            .insert("TALLYOWL_CELL__ID".into(), "us-west-1".into());
        let report = run(&inputs, None);

        assert!(report.text.contains("log.level"));
        for line in report.text.lines() {
            if line.starts_with("log.level ") {
                assert!(line.ends_with("file"), "{line}");
            }
            if line.starts_with("cell.id ") {
                assert!(line.ends_with("environment"), "{line}");
            }
            if line.starts_with("storage.tabletVoters ") {
                assert!(line.ends_with("command line"), "{line}");
            }
            if line.starts_with("integrity.mode ") {
                assert!(line.ends_with("default"), "{line}");
            }
        }
    }

    #[test]
    fn a_secret_shows_its_reference_and_never_its_value() {
        let report = check("collector:\n  apiKey: env:TALLYOWL_COLLECTOR_KEY\n");
        assert_eq!(report.exit_code, 0);
        assert!(report
            .text
            .contains("[a secret, from env:TALLYOWL_COLLECTOR_KEY]"));
    }

    #[test]
    fn an_unconfigured_secret_says_so_plainly() {
        let report = check("");
        assert!(report.text.contains("[not configured]"));
    }

    #[test]
    fn an_invalid_value_exits_non_zero_and_names_the_setting() {
        let report = check("integrity:\n  mode: paranoid\n");
        assert_eq!(report.exit_code, 1);
        assert!(report.text.contains("integrity.mode"));
        assert!(report.text.contains("verify-on-read"));
    }

    #[test]
    fn a_broken_cross_setting_rule_exits_non_zero() {
        let report = check("storage:\n  receiptPolicy: local-one\n  tabletVoters: 3\n");
        assert_eq!(report.exit_code, 1);
        assert!(report.text.contains("local-quorum"));
        // The table still prints, because a person fixing this wants to see
        // every other value at the same time.
        assert!(report.text.contains("corndogs.endpoint"));
    }

    #[test]
    fn a_key_that_matches_no_setting_fails_the_check() {
        let report = check("storage:\n  reciptPolicy: local-one\n");
        assert_eq!(report.exit_code, 1);
        assert!(report.text.contains("storage.reciptPolicy"));
        assert!(report.text.contains("spelling"));
    }

    #[test]
    fn the_report_counts_more_than_one_wrong_setting() {
        let report = check("integrity:\n  mode: paranoid\nlog:\n  level: chatty\n");
        assert!(report.text.contains("2 settings are wrong"));
    }
}
