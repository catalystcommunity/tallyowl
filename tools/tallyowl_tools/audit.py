"""The dependency audit: advisories, licenses, and where code may come from.

`docs/PLAN.md` Phase 11 requires a dependency audit, and `AGENTS.md` requires
every dependency to be compatible with Apache-2.0. Two tools split the work:

- `cargo audit` reads the lockfile against the RustSec advisory database. Its
  ignore list is `.cargo/audit.toml`, and every entry there is a ranked finding
  in the security review rather than a dismissal;
- `cargo deny` checks licenses, duplicate versions, and sources against
  `deny.toml`.

The advisory fetch writes a git reflog, and a git identity that contains angle
brackets in its name is refused by the git library `cargo audit` uses. The
override below keeps the fetch working whatever the host's identity says,
because an audit that only runs on some machines is an audit that stops
running.
"""

from __future__ import annotations

from .commands import ToolFailed, say, run, which

#: The identity the advisory fetch writes its reflog under. It names the tool,
#: because the fetch is the tool's action rather than a person's.
AUDIT_IDENTITY = {
    "GIT_AUTHOR_NAME": "tallyowl-audit",
    "GIT_AUTHOR_EMAIL": "audit@localhost",
    "GIT_COMMITTER_NAME": "tallyowl-audit",
    "GIT_COMMITTER_EMAIL": "audit@localhost",
}


def audit() -> int:
    # Fetched rather than required. The runner image carries neither of these,
    # and a job told to install something first is a job that fails first.
    from . import deps

    audit_program = deps.cargo_audit_program()
    deny_program = deps.cargo_deny_program()

    say("Checking the lockfile against the advisory database.")
    run([audit_program, "audit"], env=AUDIT_IDENTITY)
    say("Checking licenses, duplicate versions, and sources.")
    run([deny_program, "deny", "check", "licenses", "bans", "sources"])
    say("The audit passes. What it passes over is in .cargo/audit.toml, with reasons.")
    return 0
