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
pub const VERBS: [&str; 10] = [
    "snapshot",
    "restore",
    "rebuild",
    "provision",
    "key",
    "project",
    "session",
    "member",
    "token",
    "ca",
];

pub const USAGE: &str = "\
  tallyowl-head snapshot <directory>     Copy this installation into <directory>
  tallyowl-head restore <directory>      Restore a snapshot into the data directory
  tallyowl-head rebuild                  Rebuild the list of stored files by reading them

  tallyowl-head provision <project> [workspace]
                                         Create the project if it is missing, and print one new key.
                                         The workspace is `default` unless you name one, and it is
                                         created if it is missing
  tallyowl-head project list             List the workspaces and projects
  tallyowl-head key list                 List the keys, their project, and their state
  tallyowl-head key revoke <key-id>      Stop a key working
  tallyowl-head session create <name>   Sign somebody in and print one session token
  tallyowl-head session list            List the sessions and their state
  tallyowl-head session revoke <id>     End one session
  tallyowl-head member list <workspace>  List who has a role in a workspace
  tallyowl-head member add <workspace> <subject> <role>
                                         Give somebody a role: viewer, admin, or owner
  tallyowl-head member remove <workspace> <subject>
                                         Take somebody's role in a workspace away

  tallyowl-head token create <label> <role>[,<role>...] [workspace ...]
                                         Make a role token that enrolls nodes of these roles, and
                                         print it one time. A collector needs `collector-intake`,
                                         `collector-forwarder`, or both. A workspace limits what the
                                         nodes it enrolls may write; with none, they may write any
  tallyowl-head token list              List the role tokens and their state
  tallyowl-head token revoke <token-id> Stop a role token enrolling, and end the nodes it enrolled

  tallyowl-head ca create <directory>   Make a root authority and an intermediate for a first
                                         installation or a test, and print the settings to add.
                                         It needs no data directory and no running head

A <workspace> is the name or the ID that `project list` prints. A <subject> is the
account ID a person signed in with, which `session list` prints.

Each of these, except `ca create`, needs the data directory to itself. Stop the head first.
";

/// Run one verb. Returns the exit code.
pub fn run(verb: &str, rest: &[String], config_file: &str) -> i32 {
    // An authority is made before an installation exists, so this reads no
    // configuration and opens no data directory.
    if verb == "ca" {
        return ca_verb(rest);
    }
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
        "member" => member_verb(&data_dir, rest),
        "token" => token_verb(&data_dir, rest),
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
            // A workspace made now is invisible to an operator who signed in
            // before it existed, so the operators are given it here.
            let granted = store
                .catalog()
                .grant_operators(issued.key.workspace_id, tallyowl_obs::time::now_ms())
                .unwrap_or(0);
            println!("Workspace: {workspace}");
            if granted > 0 {
                println!(
                    "           {} now an owner of it.",
                    snapshot::count(granted, "signed-in operator is", "signed-in operators are")
                );
            }
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
            let workspaces = store.catalog().workspaces().map(|held| held.len());
            match store
                .catalog()
                .issue_operator_session(subject, now, OPERATOR_SESSION_MS)
            {
                Ok(issued) => {
                    match workspaces {
                        // "An owner of every workspace" was also what this said
                        // when there were none, and the person then saw nothing.
                        Ok(0) => {
                            println!("Signed in as {subject}. There is no workspace yet, so this session can see nothing.");
                            println!("Run `tallyowl-head provision <project>` next. It makes the first workspace and gives it to this session.");
                        }
                        Ok(count) => println!(
                            "Signed in as {subject}, as an owner of {}.",
                            snapshot::count(count, "workspace", "workspaces")
                        ),
                        Err(_) => {
                            println!("Signed in as {subject}, as an owner of every workspace.")
                        }
                    }
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

// ---------------------------------------------------------------------------
// Members
//
// L054: a person who signs in through LinkKeys "sees nothing until an
// administrator gives them a role". This is how an administrator gives one.
// ---------------------------------------------------------------------------

/// The workspace a person named, by its name or by its ID.
fn find_workspace(
    store: &SegmentedStore,
    named: &str,
) -> Result<tallyowl_store::control::Workspace, String> {
    let workspaces = store
        .catalog()
        .workspaces()
        .map_err(|e| format!("The workspaces could not be read: {e}"))?;
    workspaces
        .into_iter()
        .find(|workspace| workspace.name == named || hex(&workspace.workspace_id) == named)
        .ok_or_else(|| {
            format!(
                "There is no workspace named `{named}`. `tallyowl-head project list` prints \
                 the workspaces, and `tallyowl-head provision <project> {named}` makes this one."
            )
        })
}

/// The enrollments one role token permits in one hour unless an operator asks
/// for another number. Enough for an autoscaler that adds collectors in a
/// burst and for a pod that restarts in a loop, which enrolls at each start.
const DEFAULT_ENROLLMENTS_EACH_HOUR: u64 = 120;

/// `token create|list|revoke`, so that an operator can make the role token a
/// collector enrolls with (D62) without a generated control client.
fn token_verb(data_dir: &Path, rest: &[String]) -> i32 {
    use tallyowl_store::identity::{NodeRole, RoleTokenPolicy};

    let action = rest.first().map(String::as_str);
    if !matches!(action, Some("create" | "list" | "revoke")) {
        eprintln!("`token` has three actions: `create`, `list`, and `revoke`.\n\n{USAGE}");
        return 1;
    }
    // Read the words before the store opens, so a mistake costs no lock wait.
    let mut roles = Vec::new();
    if action == Some("create") {
        let (Some(label), Some(role_text)) = (rest.get(1), rest.get(2)) else {
            eprintln!(
                "`token create` needs a label and at least one role, for example `tallyowl-head token create collectors collector-intake,collector-forwarder`.\n\n{USAGE}"
            );
            return 1;
        };
        if label.trim().is_empty() {
            eprintln!("A role token needs a label that says what it is for.");
            return 1;
        }
        for text in role_text
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            match NodeRole::parse(text) {
                Some(role) => roles.push(role),
                None => {
                    eprintln!(
                        "`{text}` is not a node role. A collector uses `collector-intake` or `collector-forwarder`."
                    );
                    return 1;
                }
            }
        }
    }
    if action == Some("revoke") && rest.get(1).is_none() {
        eprintln!("`token revoke` needs the token ID that `token list` prints.\n\n{USAGE}");
        return 1;
    }

    let store = match open_store(data_dir) {
        Ok(store) => store,
        Err(code) => return code,
    };
    let now = tallyowl_obs::time::now_ms();
    match action {
        Some("create") => {
            let mut workspaces = Vec::new();
            for named in &rest[3..] {
                match find_workspace(&store, named) {
                    Ok(workspace) => workspaces.push(workspace.workspace_id),
                    Err(message) => {
                        eprintln!("{message}");
                        return 1;
                    }
                }
            }
            let policy = RoleTokenPolicy {
                roles,
                workspaces,
                enrollments_each_hour: Some(DEFAULT_ENROLLMENTS_EACH_HOUR),
                ..RoleTokenPolicy::default()
            };
            match store.catalog().issue_role_token(&rest[1], policy, now) {
                Ok(issued) => {
                    println!("{}", issued.credential);
                    eprintln!(
                        "This role token is printed one time. Put it in the Secret that `enrollment.roleToken` names. Its ID is {}, and `tallyowl-head token revoke {}` stops it.",
                        issued.token.token_id, issued.token.token_id
                    );
                    0
                }
                Err(e) => {
                    eprintln!("The role token could not be made: {e}");
                    1
                }
            }
        }
        Some("list") => {
            let tokens = store.catalog().role_tokens().unwrap_or_default();
            if tokens.is_empty() {
                println!("There is no role token. `tallyowl-head token create <label> <role>` makes one.");
                return 0;
            }
            for token in tokens {
                let state = if token.revoked_at.is_some() {
                    "revoked"
                } else {
                    "active"
                };
                let roles: Vec<&str> = token.policy.roles.iter().map(|r| r.as_str()).collect();
                println!(
                    "{}  {:7}  {:4} uses  {}  {}",
                    token.token_id,
                    state,
                    token.uses,
                    roles.join(","),
                    token.label
                );
            }
            0
        }
        Some("revoke") => {
            let token_id = &rest[1];
            match store.catalog().role_token(token_id) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    eprintln!("There is no role token `{token_id}`. `tallyowl-head token list` prints the IDs.");
                    return 1;
                }
                Err(e) => {
                    eprintln!("The role token could not be read: {e}");
                    return 1;
                }
            }
            match store.catalog().revoke_role_token(token_id, true, now) {
                Ok(nodes) => {
                    println!(
                        "Revoked role token {token_id}. {nodes} nodes it enrolled can no longer renew."
                    );
                    0
                }
                Err(e) => {
                    eprintln!("The role token could not be revoked: {e}");
                    1
                }
            }
        }
        _ => unreachable!("checked above"),
    }
}

fn member_verb(data_dir: &Path, rest: &[String]) -> i32 {
    use tallyowl_store::control::{Member, Role};

    let action = rest.first().map(String::as_str);
    if !matches!(action, Some("list" | "add" | "remove")) {
        eprintln!("`member` has three actions: `list`, `add`, and `remove`.\n\n{USAGE}");
        return 1;
    }
    let Some(named) = rest.get(1) else {
        eprintln!(
            "`member {}` needs a workspace.\n\n{USAGE}",
            action.unwrap_or_default()
        );
        return 1;
    };
    let store = match open_store(data_dir) {
        Ok(store) => store,
        Err(code) => return code,
    };
    let workspace = match find_workspace(&store, named) {
        Ok(workspace) => workspace,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };

    match action {
        Some("list") => {
            let members = store
                .catalog()
                .members(workspace.workspace_id)
                .unwrap_or_default();
            if members.is_empty() {
                println!(
                    "Nobody has a role in `{}` yet. `tallyowl-head member add {} <subject> <role>` gives one.",
                    workspace.name, workspace.name
                );
                return 0;
            }
            for member in members {
                println!("{:6}  {}", member.role.as_str(), member.subject);
            }
            0
        }
        Some("add") => {
            let (Some(subject), Some(role_text)) = (rest.get(2), rest.get(3)) else {
                eprintln!("`member add` needs a workspace, a subject, and a role.\n\n{USAGE}");
                return 1;
            };
            let Some(role) = Role::parse(role_text) else {
                eprintln!(
                    "`{role_text}` is not a role. The roles are `viewer`, `admin`, and `owner`. \
                     A viewer reads, an admin also changes settings and keys, and an owner also \
                     erases data."
                );
                return 1;
            };
            let member = Member {
                subject: subject.clone(),
                workspace_id: workspace.workspace_id,
                role,
                display_name: subject.clone(),
                added_at: tallyowl_obs::time::now_ms(),
            };
            match store.catalog().put_member(&member) {
                Ok(()) => {
                    println!(
                        "{subject} is now {} `{}` in `{}`.",
                        article(role.as_str()),
                        role.as_str(),
                        workspace.name
                    );
                    println!(
                        "It applies to the next request they make. They do not sign in again."
                    );
                    0
                }
                Err(e) => {
                    eprintln!("The role could not be stored: {e}");
                    1
                }
            }
        }
        _ => {
            let Some(subject) = rest.get(2) else {
                eprintln!("`member remove` needs a workspace and a subject.\n\n{USAGE}");
                return 1;
            };
            let held = store
                .catalog()
                .members(workspace.workspace_id)
                .unwrap_or_default();
            if !held.iter().any(|member| &member.subject == subject) {
                eprintln!(
                    "{subject} has no role in `{}`, so nothing was removed. \
                     `tallyowl-head member list {}` prints who does.",
                    workspace.name, workspace.name
                );
                return 1;
            }
            match store
                .catalog()
                .remove_member(workspace.workspace_id, subject)
            {
                Ok(()) => {
                    println!("{subject} no longer has a role in `{}`.", workspace.name);
                    println!(
                        "A session they hold stays signed in and sees nothing in this workspace."
                    );
                    0
                }
                Err(e) => {
                    eprintln!("The role could not be removed: {e}");
                    1
                }
            }
        }
    }
}

fn article(word: &str) -> &'static str {
    match word.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// The files `ca create` writes, in the order it writes them.
pub const CA_FILES: [&str; 4] = [
    "root.crt",
    "root.key",
    "intermediate.crt",
    "intermediate.key",
];

fn ca_verb(rest: &[String]) -> i32 {
    let (Some("create"), Some(directory)) = (rest.first().map(String::as_str), rest.get(1)) else {
        eprintln!("`ca` needs `create <directory>`.\n\n{USAGE}");
        return 1;
    };
    match create_authorities(Path::new(directory), tallyowl_obs::time::now_ms()) {
        Ok(settings) => {
            println!("{settings}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// Write a root and an intermediate into `directory`, and return the settings
/// that use them.
///
/// Nothing is replaced: an existing file of any of the four names stops the
/// verb before it writes anything, because a second run over a first one would
/// strand every certificate the first authority signed. Each key is readable by
/// its owner only.
pub fn create_authorities(directory: &Path, now_ms: i64) -> Result<String, String> {
    use tallyowl_store::certificates::generate_authorities;

    let existing: Vec<&str> = CA_FILES
        .iter()
        .copied()
        .filter(|name| directory.join(name).exists())
        .collect();
    if !existing.is_empty() {
        return Err(format!(
            "{} already holds {}. Nothing was written, because a new authority would replace one that may already have signed certificates. Name an empty directory.",
            directory.display(),
            existing.join(", ")
        ));
    }
    let generated = generate_authorities(now_ms).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(directory)
        .map_err(|e| format!("{} could not be created: {e}", directory.display()))?;
    let contents = [
        (CA_FILES[0], &generated.root_certificate_pem, false),
        (CA_FILES[1], &generated.root_key_pem, true),
        (CA_FILES[2], &generated.intermediate_chain_pem, false),
        (CA_FILES[3], &generated.intermediate_key_pem, true),
    ];
    for (name, text, private) in contents {
        write_new(&directory.join(name), text.as_bytes(), private)?;
    }

    let absolute = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf());
    let at = |name: &str| absolute.join(name).display().to_string();
    Ok(format!(
        "Wrote a root authority and an intermediate authority to {dir}.

Add these settings to each head:

installation:
  authorities:
    - {root}
  signingCertificate: {intermediate}
  signingKey: file:{intermediate_key}

Add these settings to each collector:

installation:
  authorities:
    - {root}

Move {root_key} to a place that no TallyOwl host can read. Nothing in TallyOwl
uses it. It is needed again only to make the next intermediate.",
        dir = absolute.display(),
        root = at(CA_FILES[0]),
        intermediate = at(CA_FILES[2]),
        intermediate_key = at(CA_FILES[3]),
        root_key = at(CA_FILES[1]),
    ))
}

/// Create a file that must not exist yet. A private file is readable by its
/// owner only, from the moment it exists.
fn write_new(path: &Path, bytes: &[u8], private: bool) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options
        .open(path)
        .map_err(|e| format!("{} could not be created: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{} could not be written: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_store::control::Role;

    fn place(name: &str) -> PathBuf {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target"));
        let path = base
            .join("admin-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    fn words(text: &[&str]) -> Vec<String> {
        text.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn ca_create_writes_a_usable_signer_and_keeps_its_keys_private() {
        use tallyowl_store::certificates::{Authority, DEFAULT_CERTIFICATE_LIFETIME_MS};
        let directory = place("ca-create");
        let now = tallyowl_obs::time::now_ms();
        let settings = create_authorities(&directory, now).expect("written");
        assert!(settings.contains("signingCertificate:"), "{settings}");
        assert!(settings.contains("signingKey: file:"), "{settings}");

        let read = |name: &str| std::fs::read_to_string(directory.join(name)).expect("written");
        let root: Vec<Vec<u8>> =
            x509_parser::pem::Pem::iter_from_buffer(read("root.crt").as_bytes())
                .map(|block| block.expect("PEM").contents)
                .collect();
        Authority::from_pem(
            &read("intermediate.crt"),
            &read("intermediate.key"),
            &root,
            now,
            DEFAULT_CERTIFICATE_LIFETIME_MS,
        )
        .expect("the files are a signer that chains to the root it wrote");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["root.key", "intermediate.key"] {
                let mode = std::fs::metadata(directory.join(name))
                    .expect("exists")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{name} is readable by others");
            }
        }
    }

    #[test]
    fn ca_create_never_replaces_an_authority_that_exists() {
        let directory = place("ca-twice");
        let now = tallyowl_obs::time::now_ms();
        create_authorities(&directory, now).expect("the first run writes");
        let before = std::fs::read(directory.join("intermediate.key")).expect("written");

        let refusal = create_authorities(&directory, now).expect_err("the second run refuses");
        assert!(refusal.contains("Nothing was written"), "{refusal}");
        assert!(refusal.contains("intermediate.key"), "{refusal}");
        assert_eq!(
            std::fs::read(directory.join("intermediate.key")).expect("kept"),
            before
        );

        // One file of the four is enough to stop it.
        let partial = place("ca-partial");
        std::fs::create_dir_all(&partial).expect("made");
        std::fs::write(partial.join("root.key"), "an operator's own key").expect("written");
        assert!(create_authorities(&partial, now).is_err());
        assert!(
            !partial.join("intermediate.crt").exists(),
            "a file was written before the refusal"
        );
    }

    #[test]
    fn ca_create_needs_no_configuration_or_data_directory() {
        let directory = place("ca-verb");
        let code = run(
            "ca",
            &words(&["create", directory.to_str().expect("a UTF-8 path")]),
            "/this/configuration/does/not/exist.yaml",
        );
        assert_eq!(code, 0);
        assert!(directory.join("root.crt").exists());
        assert_eq!(run("ca", &words(&["remove"]), "unused"), 1);
    }

    #[test]
    fn every_verb_the_usage_names_is_a_verb_the_binary_routes_here() {
        // `main` hands a verb to this module only when `VERBS` names it. A verb
        // with a function here and no entry there answered "`token` is not a
        // verb this binary has" in a real install, while its own test passed,
        // because the test called the function directly.
        for line in USAGE.lines() {
            let Some(rest) = line.trim_start().strip_prefix("tallyowl-head ") else {
                continue;
            };
            let verb = rest.split_whitespace().next().unwrap_or_default();
            assert!(
                VERBS.contains(&verb),
                "USAGE names `{verb}`, and VERBS does not, so the binary refuses it"
            );
        }
    }

    #[test]
    fn an_operator_makes_a_role_token_a_collector_can_enroll_with_and_revokes_it() {
        use tallyowl_store::identity::NodeRole;
        let data_dir = place("token");
        assert_eq!(provision_verb(&data_dir, &words(&["web", "shop"])), 0);

        // A mistake is refused before the store opens.
        assert_eq!(token_verb(&data_dir, &words(&["create", "collectors"])), 1);
        assert_eq!(
            token_verb(&data_dir, &words(&["create", "collectors", "collector"])),
            1,
            "`collector` is not a role; the refusal names the two that are"
        );
        assert_eq!(
            token_verb(
                &data_dir,
                &words(&[
                    "create",
                    "collectors",
                    "collector-intake",
                    "no-such-workspace"
                ])
            ),
            1
        );

        assert_eq!(
            token_verb(
                &data_dir,
                &words(&[
                    "create",
                    "collectors",
                    "collector-intake,collector-forwarder",
                    "shop"
                ])
            ),
            0
        );
        let token_id = {
            let store = SegmentedStore::open(&data_dir).expect("opens");
            let tokens = store.catalog().role_tokens().expect("reads");
            assert_eq!(tokens.len(), 1);
            let token = &tokens[0];
            assert_eq!(
                token.policy.roles,
                vec![NodeRole::CollectorIntake, NodeRole::CollectorForwarder]
            );
            // The workspace is the scope a collector enrolled by it may write (D32).
            let shop = find_workspace(&store, "shop").expect("the workspace");
            assert_eq!(token.policy.workspaces, vec![shop.workspace_id]);
            assert_eq!(
                token.policy.enrollments_each_hour,
                Some(DEFAULT_ENROLLMENTS_EACH_HOUR),
                "a token has an hourly limit unless an operator sets another"
            );
            assert!(token.revoked_at.is_none());
            token.token_id.clone()
        };

        assert_eq!(token_verb(&data_dir, &words(&["list"])), 0);
        assert_eq!(
            token_verb(&data_dir, &words(&["revoke", "no-such-token"])),
            1
        );
        assert_eq!(token_verb(&data_dir, &words(&["revoke", &token_id])), 0);
        let store = SegmentedStore::open(&data_dir).expect("opens");
        let revoked = store
            .catalog()
            .role_token(&token_id)
            .expect("reads")
            .expect("kept");
        assert!(revoked.revoked_at.is_some());
    }

    #[test]
    fn an_operator_gives_a_teammate_a_role_and_takes_it_away() {
        let data_dir = place("member");
        assert_eq!(provision_verb(&data_dir, &words(&["web", "shop"])), 0);

        let subject = "5f0c@id.example";
        assert_eq!(
            member_verb(&data_dir, &words(&["add", "shop", subject, "viewer"])),
            0
        );
        {
            let store = SegmentedStore::open(&data_dir).expect("opens");
            let held = store.catalog().memberships(subject).expect("reads");
            assert_eq!(held.len(), 1);
            assert_eq!(held[0].1, Role::Viewer);
        }
        assert_eq!(member_verb(&data_dir, &words(&["list", "shop"])), 0);
        assert_eq!(
            member_verb(&data_dir, &words(&["remove", "shop", subject])),
            0
        );
        let store = SegmentedStore::open(&data_dir).expect("opens");
        assert!(store
            .catalog()
            .memberships(subject)
            .expect("reads")
            .is_empty());
    }

    #[test]
    fn a_mistake_is_refused_and_changes_nothing() {
        let data_dir = place("member-refused");
        assert_eq!(provision_verb(&data_dir, &words(&["web", "shop"])), 0);
        // No such workspace, no such role, nobody to remove, and no action.
        assert_eq!(
            member_verb(&data_dir, &words(&["add", "nowhere", "a", "viewer"])),
            1
        );
        assert_eq!(
            member_verb(&data_dir, &words(&["add", "shop", "a", "root"])),
            1
        );
        assert_eq!(member_verb(&data_dir, &words(&["remove", "shop", "a"])), 1);
        assert_eq!(member_verb(&data_dir, &words(&["grant", "shop", "a"])), 1);
        assert_eq!(member_verb(&data_dir, &words(&["add", "shop", "a"])), 1);
        let store = SegmentedStore::open(&data_dir).expect("opens");
        assert!(store.catalog().memberships("a").expect("reads").is_empty());
    }

    #[test]
    fn a_workspace_provisioned_after_a_sign_in_belongs_to_that_operator() {
        let data_dir = place("provision-grants");
        assert_eq!(session_verb(&data_dir, &words(&["create", "ada"])), 0);
        assert_eq!(provision_verb(&data_dir, &words(&["web", "later"])), 0);
        let store = SegmentedStore::open(&data_dir).expect("opens");
        let held = store.catalog().memberships("ada").expect("reads");
        assert_eq!(
            held.len(),
            1,
            "the operator cannot see the workspace they just made"
        );
        assert_eq!(held[0].1, Role::Owner);
    }
}
