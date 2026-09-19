//! The recovery verbs an operator runs when the head is stopped.
//!
//! `docs/FAILURE_MODES.md` section 11 describes six procedures. Three of them
//! are commands rather than automatic behaviour, and this is where they live:
//! `snapshot`, `restore`, and `rebuild`.
//!
//! Every one of them needs the data directory to itself, because one process
//! owns one data directory. They therefore run in this binary and not through a
//! request to a running head: an operator stops the head, runs the verb, reads
//! what it says, and starts the head again.
//!
//! Each verb prints what it did in words an operator can act on. A rebuild in
//! particular says what it could **not** put back, because that list is the
//! part somebody has to do something about.

use std::path::{Path, PathBuf};

use tallyowl_config::Config;
use tallyowl_store::row::hex;
use tallyowl_store::segmented::SegmentedStore;
use tallyowl_store::snapshot;

/// The verbs this module answers.
pub const VERBS: [&str; 7] = [
    "snapshot",
    "restore",
    "rebuild",
    "provision",
    "key",
    "project",
    "session",
];

pub const USAGE: &str = "\
  tallyowl-head snapshot <directory>     Copy this installation into <directory>
  tallyowl-head restore <directory>      Restore a snapshot into the data directory
  tallyowl-head rebuild                  Rebuild the list of stored files by reading them

  tallyowl-head provision <project>      Create the project if it is missing, and print one new key
  tallyowl-head project list             List the workspaces and projects
  tallyowl-head key list                 List the keys, their project, and their state
  tallyowl-head key revoke <key-id>      Stop a key working
  tallyowl-head session create <name>   Sign somebody in and print one session token
  tallyowl-head session list            List the sessions and their state
  tallyowl-head session revoke <id>     End one session

Each of these needs the data directory to itself. Stop the head first.
";

/// Run one verb. Returns the exit code.
pub fn run(verb: &str, rest: &[String], config_file: &str) -> i32 {
    let data_dir = match data_directory(config_file) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };

    match verb {
        "snapshot" => snapshot_verb(&data_dir, rest),
        "restore" => restore_verb(&data_dir, rest),
        "rebuild" => rebuild_verb(&data_dir),
        "provision" => provision_verb(&data_dir, rest),
        "project" => project_verb(&data_dir, rest),
        "key" => key_verb(&data_dir, rest),
        "session" => session_verb(&data_dir, rest),
        _ => {
            eprintln!("`{verb}` is not a verb this binary has.\n\n{USAGE}");
            1
        }
    }
}

fn data_directory(config_file: &str) -> Result<PathBuf, String> {
    let config = Config::load_from_host(config_file).map_err(|errors| {
        let mut message = String::from("The configuration could not be read.\n");
        for error in &errors {
            message.push_str(&format!("  {}\n", error.message));
        }
        message
    })?;
    Ok(PathBuf::from(config.text("head.dataDir")))
}

fn snapshot_verb(data_dir: &Path, rest: &[String]) -> i32 {
    let Some(into) = rest.first() else {
        eprintln!("`snapshot` needs a directory to write into.\n\n{USAGE}");
        return 1;
    };
    let into = PathBuf::from(into);

    // The snapshot has to seal the open buffer, so it opens the store rather
    // than reading the directory from outside. A head that is still running
    // holds the lock, and the message below says so.
    let store = match SegmentedStore::open(data_dir) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("The data directory could not be opened: {e}");
            return 1;
        }
    };

    match store.snapshot(&into, tallyowl_obs::time::now_ms()) {
        Ok(taken) => {
            println!(
                "Copied {} and {} into {}.",
                snapshot::count(taken.segments.len(), "stored file", "stored files"),
                snapshot::count(taken.erasures, "erasure record", "erasure records"),
                into.display()
            );
            println!(
                "The snapshot covers everything committed up to number {}.",
                taken.commit_watermark
            );
            println!("Run this again into the same directory to add only what is new.");
            0
        }
        Err(e) => {
            eprintln!("The snapshot did not finish: {e}");
            1
        }
    }
}

fn restore_verb(data_dir: &Path, rest: &[String]) -> i32 {
    let Some(from) = rest.first() else {
        eprintln!("`restore` needs the directory that holds the snapshot.\n\n{USAGE}");
        return 1;
    };
    let from = PathBuf::from(from);

    // Restoring on top of an installation that already holds data would mix two
    // installations together, and there is no way to take that back.
    if holds_data(data_dir) {
        eprintln!(
            "The data directory {} already holds data. Restore writes into an empty \
             directory, so that a restore can never mix two installations together. \
             Move the existing directory aside first.",
            data_dir.display()
        );
        return 1;
    }

    match snapshot::restore(&from, data_dir) {
        Ok(report) if report.is_complete() => {
            println!(
                "Restored {} and {} into {}.",
                snapshot::count(report.segments_restored, "stored file", "stored files"),
                snapshot::count(
                    report.erasures_restored,
                    "erasure record",
                    "erasure records"
                ),
                data_dir.display()
            );
            println!("Nobody who asked to be removed has come back.");
            0
        }
        Ok(report) => {
            // Section 13: a missing or corrupt file causes a visible failure,
            // and nothing was published.
            eprintln!("The restore did not run, and the data directory was not changed.");
            for name in &report.segments_missing {
                eprintln!("  This file is named in the snapshot and is not there: {name}");
            }
            for name in &report.segments_damaged {
                eprintln!("  This file is damaged: {name}");
            }
            eprintln!(
                "\nA restore that skipped a file would give wrong answers and say nothing, \
                 so it stops instead. Use another copy of the snapshot."
            );
            1
        }
        Err(e) => {
            eprintln!("The restore did not run: {e}");
            1
        }
    }
}

fn rebuild_verb(data_dir: &Path) -> i32 {
    match snapshot::rebuild(data_dir) {
        Ok(report) => {
            print!("{}", report.to_text());
            if report.segments_unreadable.is_empty() {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("The rebuild did not finish: {e}");
            1
        }
    }
}

fn holds_data(data_dir: &Path) -> bool {
    let segments = data_dir.join("segments");
    let has_segments = std::fs::read_dir(&segments)
        .map(|entries| entries.flatten().any(|entry| entry.path().is_file()))
        .unwrap_or(false);
    has_segments || data_dir.join("catalog/catalog.redb").is_file()
}

// ---------------------------------------------------------------------------
// The control verbs
//
// A key is created by an operator, printed once, and never stored in a form
// anything can read back. These run in this binary for the same reason the
// recovery verbs do: one process owns one data directory, and the control
// catalog lives in it.
// ---------------------------------------------------------------------------

fn open_store(data_dir: &Path) -> Result<SegmentedStore, i32> {
    match SegmentedStore::open(data_dir) {
        Ok(store) => Ok(store),
        Err(e) => {
            eprintln!("The data directory could not be opened: {e}");
            eprintln!("Stop the head first. One process owns one data directory.");
            Err(1)
        }
    }
}

fn provision_verb(data_dir: &Path, rest: &[String]) -> i32 {
    let Some(project) = rest.first() else {
        eprintln!("`provision` needs a project name.\n\n{USAGE}");
        return 1;
    };
    let workspace = rest
        .get(1)
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    let store = match open_store(data_dir) {
        Ok(store) => store,
        Err(code) => return code,
    };
    match store
        .catalog()
        .provision(&workspace, project, tallyowl_obs::time::now_ms())
    {
        Ok(issued) => {
            println!("Workspace: {workspace}");
            println!("Project:   {project} ({})", hex(&issued.key.project_id));
            println!("Key:       {}", issued.key.key_id);
            println!();
            println!("{}", issued.credential);
            println!();
            println!(
                "This is the only time the key is shown. TallyOwl stores a digest of it \
                 and cannot print it again."
            );
            println!("Put it where `collector.apiKey` points, as a `file:` or `env:` reference.");
            0
        }
        Err(e) => {
            eprintln!("The project could not be created: {e}");
            1
        }
    }
}

fn project_verb(data_dir: &Path, rest: &[String]) -> i32 {
    if rest.first().map(String::as_str) != Some("list") {
        eprintln!("`project` has one action, `list`.\n\n{USAGE}");
        return 1;
    }
    let store = match open_store(data_dir) {
        Ok(store) => store,
        Err(code) => return code,
    };
    let workspaces = store.catalog().workspaces().unwrap_or_default();
    let projects = store.catalog().projects().unwrap_or_default();
    if projects.is_empty() {
        println!("There are no projects yet. Run `tallyowl-head provision <name>` to make one.");
        return 0;
    }
    for workspace in &workspaces {
        println!("{}  {}", hex(&workspace.workspace_id), workspace.name);
        for project in projects
            .iter()
            .filter(|p| p.workspace_id == workspace.workspace_id)
        {
            println!("  {}  {}", hex(&project.project_id), project.name);
        }
    }
    0
}

fn key_verb(data_dir: &Path, rest: &[String]) -> i32 {
    let store = match open_store(data_dir) {
        Ok(store) => store,
        Err(code) => return code,
    };
    match rest.first().map(String::as_str) {
        Some("list") => {
            let projects = store.catalog().projects().unwrap_or_default();
            let keys = store.catalog().api_keys().unwrap_or_default();
            if keys.is_empty() {
                println!("There are no keys yet.");
                return 0;
            }
            let now = tallyowl_obs::time::now_ms();
            for key in keys {
                let project = projects
                    .iter()
                    .find(|p| p.project_id == key.project_id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| hex(&key.project_id));
                let state = if key.revoked_at.is_some() {
                    "revoked"
                } else if key.expires_at.is_some_and(|at| now >= at) {
                    "expired"
                } else {
                    "active"
                };
                println!("{}  {state:7}  {project}  {}", key.key_id, key.label);
            }
            0
        }
        Some("revoke") => {
            let Some(key_id) = rest.get(1) else {
                eprintln!("`key revoke` needs the key identifier that `key list` prints.");
                return 1;
            };
            match store
                .catalog()
                .revoke_api_key(key_id, tallyowl_obs::time::now_ms())
            {
                Ok(true) => {
                    println!("The key {key_id} no longer works.");
                    println!(
                        "A collector may hold its answer for a few more seconds. Every other \
                         key for that source is unaffected."
                    );
                    0
                }
                Ok(false) => {
                    eprintln!("There is no key with the identifier {key_id}.");
                    1
                }
                Err(e) => {
                    eprintln!("The key could not be revoked: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("`key` has two actions, `list` and `revoke`.\n\n{USAGE}");
            1
        }
    }
}

/// How long a session an operator issues lasts.
///
/// A day, because a session is a person's credential and a person comes back
/// tomorrow. A LinkKeys sign-in sets its own lifetime from the assertion.
const OPERATOR_SESSION_MS: i64 = 24 * 60 * 60 * 1_000;

fn session_verb(data_dir: &Path, rest: &[String]) -> i32 {
    let store = match open_store(data_dir) {
        Ok(store) => store,
        Err(code) => return code,
    };
    let now = tallyowl_obs::time::now_ms();
    match rest.first().map(String::as_str) {
        Some("create") => {
            let Some(subject) = rest.get(1) else {
                eprintln!("`session create` needs a name for whoever is signing in.");
                return 1;
            };
            // An operator session is an owner of every workspace that exists
            // now. It is not a way past authorization: it writes a real
            // membership and issues a real session, and `session list` and
            // `session revoke` see both.
            match store
                .catalog()
                .issue_operator_session(subject, now, OPERATOR_SESSION_MS)
            {
                Ok(issued) => {
                    println!("Signed in as {subject}, as an owner of every workspace.");
                    println!("Session:   {}", issued.record.session_id);
                    println!("Ends:      in 24 hours");
                    println!();
                    println!("{}", issued.token);
                    println!();
                    println!(
                        "This is the only time the token is shown. TallyOwl stores a digest of \
                         it and cannot print it again."
                    );
                    println!(
                        "LinkKeys owns human authentication. This command exists for an \
                         installation that has no LinkKeys domain configured yet, and for the \
                         first sign-in of one that does."
                    );
                    0
                }
                Err(e) => {
                    eprintln!("The session could not be made: {e}");
                    1
                }
            }
        }
        Some("list") => {
            let sessions = store.catalog().sessions().unwrap_or_default();
            if sessions.is_empty() {
                println!("Nobody is signed in.");
                return 0;
            }
            for session in sessions {
                let state = if session.revoked_at.is_some() {
                    "revoked"
                } else if now >= session.expires_at {
                    "expired"
                } else {
                    "active"
                };
                println!(
                    "{}  {state:7}  {}  from {}",
                    session.session_id, session.subject, session.issuer
                );
            }
            0
        }
        Some("revoke") => {
            let Some(session_id) = rest.get(1) else {
                eprintln!(
                    "`session revoke` needs the session identifier that `session list` prints."
                );
                return 1;
            };
            match store.catalog().revoke_session(session_id, now) {
                Ok(true) => {
                    println!("The session {session_id} has ended.");
                    0
                }
                Ok(false) => {
                    eprintln!("There is no session with the identifier {session_id}.");
                    1
                }
                Err(e) => {
                    eprintln!("The session could not be ended: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("`session` has three actions: `create`, `list`, and `revoke`.\n\n{USAGE}");
            1
        }
    }
}
