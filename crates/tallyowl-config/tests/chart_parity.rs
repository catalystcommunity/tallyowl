//! The chart and the loader must agree.
//!
//! A rendered chart and a local configuration drift the moment somebody adds a
//! setting to one of them. This test therefore reads the `home` profile values
//! from both charts and from the committed example, and asserts that they and
//! the loader agree on every key, every type, and every default.
//!
//! **This is the test that keeps the local development loop matching a
//! deployment**, rather than slowly becoming a development-only arrangement.
//! `docs/PLAN.md` Phase 1 and `docs/DEPLOYMENT.md` section 8 both name it.
//!
//! It fails when a setting reaches only one of them, which is the point.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tallyowl_config::loader::{flatten_yaml, Inputs};
use tallyowl_config::value::Kind;
use tallyowl_config::{Config, SCHEMA};

/// Top-level keys in a chart's values that are deployment concerns rather than
/// TallyOwl settings. The loader has no opinion about a container image.
const NOT_SETTINGS: &[&str] = &[
    "image",
    "replicas",
    "resources",
    "persistence",
    // Phase 7 placement. Where a pod runs and how many may be away at once are
    // scheduler concerns, and TallyOwl has no setting for either: it reads its
    // own failure domain from `node.failureDomain`, which the chart sets from
    // the node the pod landed on.
    "topologySpread",
    "affinity",
    "disruption",
    // Phase 11 deployment concerns. How many collector pods run is the
    // scheduler's question; what each one does is TallyOwl's. The Corndogs
    // beside the head is a container the chart runs, and the `corndogs`
    // settings above describe how to reach it either way.
    "autoscaling",
    "corndogsDeployment",
    // The release candidate's image ships a fixed non-root identity, and which
    // user a pod runs as is the cluster's question rather than a TallyOwl
    // setting. See the Containerfile.
    "securityContext",
    // What a pod binds and where the image keeps the dashboard bundle. The
    // settings below keep the loader's own defaults — loopback, and the path a
    // developer builds into — and the chart's `settings` helper rewrites them
    // for a pod. A pod that binds loopback reaches nothing.
    "bindAddress",
    "dashboardAssets",
    // Where a cluster routes traffic from. A Gateway is the cluster's object,
    // not TallyOwl's.
    "gateway",
];

fn repository_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is the crate; the repository is two levels above it.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repository_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} could not be read: {e}", path.display()))
}

/// The settings a values document declares, with the deployment keys removed.
fn settings_in(relative: &str) -> std::collections::BTreeMap<String, String> {
    flatten_yaml(&read(relative))
        .unwrap_or_else(|e| panic!("{relative} is not valid YAML: {}", e.message))
        .into_iter()
        .filter(|(key, _)| {
            let head = key.split('.').next().unwrap_or_default();
            !NOT_SETTINGS.contains(&head)
        })
        .collect()
}

const DOCUMENTS: &[&str] = &[
    "tallyowl.example.yaml",
    "charts/tallyowl/values.yaml",
    "charts/tallyowl-collector/values.yaml",
];

#[test]
fn every_document_declares_every_setting_the_loader_knows() {
    let expected: BTreeSet<&str> = SCHEMA.iter().map(|s| s.path).collect();

    for document in DOCUMENTS {
        let declared: BTreeSet<String> = settings_in(document).keys().cloned().collect();

        let missing: Vec<&&str> = expected
            .iter()
            .filter(|path| !declared.contains(**path))
            .collect();
        assert!(
            missing.is_empty(),
            "{document} does not declare {missing:?}. A setting that reaches the \
             loader and not the chart is how the development loop stops matching \
             a deployment. Add it to every document in DOCUMENTS."
        );

        let unknown: Vec<&String> = declared
            .iter()
            .filter(|path| !expected.contains(path.as_str()))
            .collect();
        assert!(
            unknown.is_empty(),
            "{document} declares {unknown:?}, which the loader does not know. \
             Either add it to the schema in crates/tallyowl-config/src/schema.rs, \
             or remove it: a value nobody reads changes nothing."
        );
    }
}

#[test]
fn every_document_agrees_with_the_loader_on_every_type() {
    for document in DOCUMENTS {
        for (path, text) in settings_in(document) {
            let setting = tallyowl_config::schema::find(&path)
                .unwrap_or_else(|| panic!("{document} declares the unknown key {path}"));
            // An empty secret is "not configured", which is a valid home value.
            if setting.kind == Kind::Secret && text.is_empty() {
                continue;
            }
            tallyowl_config::value::parse(&setting.kind, &text).unwrap_or_else(|e| {
                panic!(
                    "{document} gives `{path}` the value `{text}`, and the loader \
                     reads that setting as {}: {}",
                    setting.kind.name(),
                    e.reason
                )
            });
        }
    }
}

#[test]
fn every_document_agrees_with_the_loader_on_every_default() {
    // A chart that ships a different default from the built-in one means an
    // operator reading DEPLOYMENT.md and a developer reading the code disagree
    // about what happens, and both of them are reading a true document.
    for document in DOCUMENTS {
        let declared = settings_in(document);
        for setting in SCHEMA {
            let Some(text) = declared.get(setting.path) else {
                continue;
            };
            if setting.kind == Kind::Secret {
                continue;
            }
            let from_document = tallyowl_config::value::parse(&setting.kind, text)
                .expect("the type test covers this");
            let built_in = tallyowl_config::value::parse(&setting.kind, setting.default)
                .expect("the schema test covers this");
            assert_eq!(
                from_document, built_in,
                "{document} gives `{}` the value `{text}` and the built-in default \
                 is `{}`. Change both, or neither.",
                setting.path, setting.default
            );
        }
    }
}

#[test]
fn the_two_charts_agree_with_each_other() {
    // Both charts render the same configuration document, because there is one
    // loader. A key that differs between them is a real deployment defect: two
    // releases of one installation would disagree about a shared value.
    let head = settings_in("charts/tallyowl/values.yaml");
    let collector = settings_in("charts/tallyowl-collector/values.yaml");
    assert_eq!(
        head, collector,
        "the two charts disagree. Both install one installation, so a shared \
         setting must have one value."
    );
}

#[test]
fn the_home_profile_starts_a_valid_installation() {
    // The documents are not only well formed; the configuration they describe
    // passes every cross-setting rule. A chart that renders a refused
    // configuration fails at the pod rather than at the render.
    for document in DOCUMENTS {
        let mut inputs = Inputs {
            file: flatten_yaml(&read(document)).expect("valid YAML"),
            ..Inputs::default()
        };
        // The deployment keys are not settings, and the loader reports an
        // unknown key rather than ignoring it, so they come out first.
        inputs
            .file
            .retain(|key, _| !NOT_SETTINGS.contains(&key.split('.').next().unwrap_or_default()));

        let config = Config::from_inputs(&inputs).unwrap_or_else(|errors| {
            let reasons: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            panic!(
                "{document} does not describe a valid installation:\n  {}",
                reasons.join("\n  ")
            )
        });
        assert_eq!(config.text("installation.profile"), "home");
        assert_eq!(config.text("storage.receiptPolicy"), "local-one");
    }
}

#[test]
fn a_setting_added_to_only_one_document_is_caught() {
    // The test above passes today. This one proves it would fail tomorrow, by
    // building the comparison the same way against a document with one key
    // removed. Without this, a parity test that quietly stopped comparing would
    // look exactly like a parity test that passes.
    let mut declared: BTreeSet<String> = settings_in("charts/tallyowl/values.yaml")
        .keys()
        .cloned()
        .collect();
    assert!(declared.remove("storage.receiptPolicy"));

    let missing: Vec<&str> = SCHEMA
        .iter()
        .map(|s| s.path)
        .filter(|path| !declared.contains(*path))
        .collect();
    assert_eq!(
        missing,
        vec!["storage.receiptPolicy"],
        "the comparison must notice a key that a document does not declare"
    );
}
