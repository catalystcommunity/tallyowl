"""The local development loop.

**The development loop is the home profile.** A developer runs one head, one
collector, and one Corndogs, which is the smallest supported production
deployment. There is no development mode, no mock, and no in-process shortcut
between the collector and the head. See docs/PLAN.md Phase 1.

**Supervision lives here, never in a service.** A service that knows how to start
its siblings has a development code path, and this phase exists to prevent that.

`--without <service>` is the debug loop: start the rest from here and run the
third in a debugger, with no container indirection and no attach dance.
"""

from __future__ import annotations

import json
import os
import re
import signal
import socket
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

from .commands import REPOSITORY_ROOT, ToolFailed, say, warn, which

RUN_DIR = REPOSITORY_ROOT / "run"
DATA_DIR = REPOSITORY_ROOT / "data"
CONFIG_FILE = REPOSITORY_ROOT / "tallyowl.local.yaml"
EXAMPLE_CONFIG = REPOSITORY_ROOT / "tallyowl.example.yaml"
#: Where the local key lands. Git ignores the data directory.
KEY_FILE = DATA_DIR / "collector.key"
#: Where the local operator session lands. A query needs one: every control
#: operation checks authorization.
SESSION_FILE = DATA_DIR / "operator.session"
#: The project a local installation writes to.
LOCAL_PROJECT = "local"

#: The order matters. Corndogs first, because the collector refuses to start
#: without it, which is the behaviour we want rather than a race to paper over.
SERVICES = ("corndogs", "head", "collector")


@dataclass
class Process:
    name: str
    pid: int
    log: Path


def _state_file() -> Path:
    return RUN_DIR / "processes.json"


def _read_state() -> list[Process]:
    path = _state_file()
    if not path.exists():
        return []
    raw = json.loads(path.read_text())
    return [Process(name=p["name"], pid=p["pid"], log=Path(p["log"])) for p in raw]


def _write_state(processes: list[Process]) -> None:
    RUN_DIR.mkdir(parents=True, exist_ok=True)
    _state_file().write_text(
        json.dumps(
            [{"name": p.name, "pid": p.pid, "log": str(p.log)} for p in processes],
            indent=2,
        )
    )


def _is_running(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _wait_for_port(address: str, seconds: float) -> bool:
    host, _, port = address.rpartition(":")
    host = host or "127.0.0.1"
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, int(port)), timeout=0.25):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def _corndogs_command() -> tuple[list[str], Path]:
    """How to start Corndogs, and where to run it.

    A binary on the path wins. Otherwise a checkout beside this repository runs
    from source, which is what a developer here normally has. `go run main.go`
    only finds its module from inside that checkout, so the directory travels
    with the command rather than being assumed.
    """
    binary = which("corndogs")
    if binary:
        return [binary, "run"], REPOSITORY_ROOT

    checkout = Path(
        os.environ.get(
            "CORNDOGS_REPO", str(REPOSITORY_ROOT.parent / "corndogs" / "corndogs")
        )
    )
    if (checkout / "main.go").exists():
        go = which("go")
        if not go:
            raise ToolFailed(
                "Corndogs is available as source and `go` is not installed. "
                "Install Go, or put a `corndogs` binary on the path."
            )
        return [go, "run", "main.go", "run"], checkout

    raise ToolFailed(
        "Corndogs is not installed and there is no checkout beside this "
        "repository. Put a `corndogs` binary on the path, or set CORNDOGS_REPO "
        "to a checkout."
    )


def _corndogs_environment() -> dict[str, str]:
    """Corndogs settings the home profile needs.

    The flush mode is the one that matters. `interval` and `never` acknowledge
    writes that a power loss can destroy, and every TallyOwl receipt rests on
    this setting. See DELIVERY.md section 1.
    """
    return {
        "STORAGE_BACKEND": "file",
        "CORNDOGS_FILESTORE_DIR": str(DATA_DIR / "corndogs"),
        "CORNDOGS_FILESTORE_SYNC": "group",
        "CORNDOGS_LISTEN": "127.0.0.1:5080",
        "CORNDOGS_HTTP_LISTEN": "127.0.0.1:5081",
        "LOGLEVEL": "error",
    }


def _service_command(
    name: str,
) -> tuple[list[str], dict[str, str], str | None, Path]:
    """The command, the environment, the address to wait for, and the directory."""
    if name == "corndogs":
        command, directory = _corndogs_command()
        return command, _corndogs_environment(), "127.0.0.1:5080", directory

    binary = _binary(f"tallyowl-{name}")
    if not binary.exists():
        raise ToolFailed(
            f"tallyowl-{name} is not built. Run `./tools.sh build` first."
        )
    command = [str(binary), "--config", str(CONFIG_FILE)]
    address = "127.0.0.1:5110" if name == "head" else "127.0.0.1:5100"
    return command, {}, address, REPOSITORY_ROOT


def _binary(name: str) -> Path:
    """The built binary to run.

    A release build by default when one exists, because a measurement taken
    against a debug build measures the debug build. `TALLYOWL_PROFILE=debug`
    forces the other one, which is what a debugger wants.

    **A release build that is older than the debug build is named, loudly.**
    `./tools.sh build` builds debug, so a developer who built and then started
    the loop would silently run whatever release binary happened to be lying
    about. That has already cost this project a day once: L074 records a load
    run whose discrepancy was the development loop rather than TallyOwl. The
    warning says which binary is running and how to fix it, and it does not
    choose for the developer, because a person measuring on purpose wants the
    release binary and a person debugging wants to be told.
    """
    wanted = os.environ.get("TALLYOWL_PROFILE")
    order = [wanted] if wanted else ["release", "debug"]
    chosen = None
    for profile in order:
        candidate = REPOSITORY_ROOT / "target" / profile / name
        if candidate.exists():
            chosen = candidate
            break
    if chosen is None:
        return REPOSITORY_ROOT / "target" / "debug" / name

    other = REPOSITORY_ROOT / "target" / ("debug" if chosen.parent.name == "release" else "release") / name
    if other.exists() and other.stat().st_mtime > chosen.stat().st_mtime + 1:
        warn(
            f"Running the {chosen.parent.name} build of {name}, and the "
            f"{other.parent.name} build is newer."
        )
        warn(
            "`./tools.sh build` builds debug. Run `cargo build --release "
            "--workspace` to bring the release build up to date, or set "
            "TALLYOWL_PROFILE=debug to run what you just built."
        )
    return chosen


def up(without: list[str] | None = None) -> int:
    """Start the home profile and follow its logs."""
    skipped = set(without or [])
    for name in skipped:
        if name not in SERVICES:
            raise ToolFailed(
                f"`{name}` is not one of the services. Use one of {', '.join(SERVICES)}."
            )

    if not CONFIG_FILE.exists():
        say("No local configuration. Writing one from the example.")
        setup_configuration()

    RUN_DIR.mkdir(parents=True, exist_ok=True)
    DATA_DIR.mkdir(parents=True, exist_ok=True)

    # TallyOwl issues keys, so the loop asks for one rather than inventing a
    # credential. This runs before the head starts, because one process owns
    # one data directory and `provision` needs it to itself. It is the same
    # command an operator runs; the loop only saves the typing.
    if "head" not in skipped:
        _provision_key()
        _provision_session()

    existing = [p for p in _read_state() if _is_running(p.pid)]
    if existing:
        raise ToolFailed(
            "Some of these are already running. Run `./tools.sh dev down` first."
        )

    started: list[Process] = []
    for name in SERVICES:
        if name in skipped:
            say(f"Not starting {name}. Run it yourself, in a debugger if you want one.")
            continue
        command, environment, address, directory = _service_command(name)
        log = RUN_DIR / f"{name}.log"
        handle = log.open("wb")
        say(f"Starting {name}")
        process = subprocess.Popen(  # noqa: S603 - an array, never a shell string
            command,
            # Its own process group, so `down` can stop the whole tree.
            #
            # Corndogs runs as `go run main.go run` when there is no binary on
            # the path, and `go run` compiles to a temporary executable and runs
            # it as a **child**. `Popen` therefore records the wrapper's
            # identifier, and stopping the wrapper leaves the server running.
            #
            # That happened, and it cost a day. An orphaned Corndogs holds the
            # delivery queue open across every `rm -rf data/` — the file is
            # unlinked and the process keeps the inode — so the next collector
            # inherits tasks it never accepted, and a wiped head commits them
            # all as new. It looked exactly like a duplication defect in
            # TallyOwl and it was this.
            start_new_session=True,
            cwd=str(directory),
            env={**os.environ, **environment},
            stdout=handle,
            stderr=subprocess.STDOUT,
            shell=False,
        )
        started.append(Process(name=name, pid=process.pid, log=log))
        _write_state(started)

        if address and not _wait_for_port(address, 20.0):
            warn(f"{name} did not start listening on {address}. Its log says:")
            warn(log.read_text()[-2000:])
            down()
            raise ToolFailed(f"{name} did not start.")

    if not started:
        raise ToolFailed("Every service was skipped, so there is nothing to start.")

    say("Running. Follow the logs with `./tools.sh dev logs`, and stop with `./tools.sh dev down`.")
    for process in started:
        print(f"  {process.name}: {process.log.relative_to(REPOSITORY_ROOT)}")
    return 0


def _provision_key() -> None:
    """Make one source key, if this installation has none yet.

    The key is printed once by the head and never again, so it is written
    straight to a file that `collector.apiKey` points at. A secret is a
    reference in the configuration, never a value.
    """
    if KEY_FILE.exists() and KEY_FILE.read_text().strip():
        return
    binary = _binary("tallyowl-head")
    if not binary.exists():
        raise ToolFailed("tallyowl-head is not built. Run `./tools.sh build` first.")

    DATA_DIR.mkdir(parents=True, exist_ok=True)
    say(f"Making a key for the `{LOCAL_PROJECT}` project.")
    completed = subprocess.run(  # noqa: S603 - an array, never a shell string
        [str(binary), "--config", str(CONFIG_FILE), "provision", LOCAL_PROJECT],
        cwd=str(REPOSITORY_ROOT),
        capture_output=True,
        text=True,
        shell=False,
    )
    if completed.returncode != 0:
        warn(completed.stdout)
        warn(completed.stderr)
        raise ToolFailed("The key could not be made.")

    credential = next(
        (line.strip() for line in completed.stdout.splitlines() if line.startswith("tow_")),
        "",
    )
    if not credential:
        warn(completed.stdout)
        raise ToolFailed("The head did not print a key.")

    KEY_FILE.write_text(credential + "\n")
    KEY_FILE.chmod(0o600)
    say(f"Wrote the key to {KEY_FILE.relative_to(REPOSITORY_ROOT)}.")


def _provision_session() -> None:
    """Sign an operator in, if this installation has no session yet.

    Every control operation checks authorization, so a query needs a session.
    LinkKeys owns human authentication; this is the command an installation with
    no LinkKeys domain runs for its first sign-in, and it is the same command an
    operator types.
    """
    if SESSION_FILE.exists() and SESSION_FILE.read_text().strip():
        return
    binary = _binary("tallyowl-head")
    if not binary.exists():
        raise ToolFailed("tallyowl-head is not built. Run `./tools.sh build` first.")

    say("Signing in a local operator.")
    completed = subprocess.run(  # noqa: S603 - an array, never a shell string
        [str(binary), "--config", str(CONFIG_FILE), "session", "create", "operator"],
        cwd=str(REPOSITORY_ROOT),
        capture_output=True,
        text=True,
        shell=False,
    )
    if completed.returncode != 0:
        warn(completed.stdout)
        warn(completed.stderr)
        raise ToolFailed("The session could not be made.")

    token = next(
        (line.strip() for line in completed.stdout.splitlines() if line.startswith("tos_")),
        "",
    )
    if not token:
        warn(completed.stdout)
        raise ToolFailed("The head did not print a session token.")

    SESSION_FILE.write_text(token + "\n")
    SESSION_FILE.chmod(0o600)
    say(f"Wrote the session to {SESSION_FILE.relative_to(REPOSITORY_ROOT)}.")


def down(remove_data: bool = False) -> int:
    """Stop the services. The data directory stays unless asked."""
    processes = _read_state()
    if not processes:
        say("Nothing is running.")
    for process in reversed(processes):
        if not _is_running(process.pid):
            continue
        say(f"Stopping {process.name}")
        # The group, not the process. See the note in `up`.
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            try:
                os.kill(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                continue
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if not any(_is_running(p.pid) for p in processes):
            break
        time.sleep(0.1)
    for process in processes:
        if _is_running(process.pid):
            warn(f"{process.name} did not stop, so it is being ended.")
            try:
                os.killpg(os.getpgid(process.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                try:
                    os.kill(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    if _state_file().exists():
        _state_file().unlink()

    # A service this command did not start still holds its address. Say so
    # loudly: a survivor keeps its state, and a durable queue that outlives a
    # `dev down` hands the next run tasks from the last one.
    for name, address in _service_addresses().items():
        holder = _listening_on(address)
        if holder is not None:
            warn(
                f"Something is still listening on {address}, which is {name}'s "
                f"address. This command did not start it, so it did not stop "
                f"it. End process {holder} before starting again, or the next "
                f"run inherits whatever this one left behind."
            )

    if remove_data:
        import shutil

        for directory in (DATA_DIR, RUN_DIR):
            if directory.exists():
                say(f"Removing {directory.relative_to(REPOSITORY_ROOT)}")
                shutil.rmtree(directory)
    return 0


def _service_addresses() -> dict[str, str]:
    """Every address the loop's services listen on."""
    return {
        "corndogs": "127.0.0.1:5080",
        "head": "127.0.0.1:5110",
        "collector": "127.0.0.1:5100",
    }


def _listening_on(address: str) -> int | None:
    """The process identifier listening on `address`, when one is.

    A bind test alone would say "busy" without saying who, and the whole point
    of this check is to name the survivor.
    """
    host, _, port = address.rpartition(":")
    try:
        out = subprocess.run(  # noqa: S603 - an array, never a shell string
            ["ss", "-lptnH", f"sport = :{port}"],
            capture_output=True,
            text=True,
            check=False,
        ).stdout
    except FileNotFoundError:
        return None
    del host
    match = re.search(r"pid=(\d+)", out)
    return int(match.group(1)) if match else None


def logs(follow: bool = True) -> int:
    """Follow every service log."""
    processes = _read_state()
    if not processes:
        raise ToolFailed("Nothing is running. Start it with `./tools.sh dev up`.")
    tail = which("tail")
    if not tail:
        for process in processes:
            print(f"--- {process.name} ---")
            print(process.log.read_text())
        return 0
    command = [tail, "-n", "40"]
    if follow:
        command.append("-f")
    command.extend(str(p.log) for p in processes)
    try:
        subprocess.run(command, check=False, shell=False)  # noqa: S603
    except KeyboardInterrupt:
        pass
    return 0


def status() -> int:
    processes = _read_state()
    if not processes:
        say("Nothing is running.")
        return 0
    for process in processes:
        state = "running" if _is_running(process.pid) else "stopped"
        print(f"  {process.name:<12} {state:<9} pid {process.pid}")
    return 0


def setup_configuration() -> None:
    """Write a local configuration file from the committed example."""
    if CONFIG_FILE.exists():
        say(f"{CONFIG_FILE.name} already exists. Leaving it alone.")
        return
    if not EXAMPLE_CONFIG.exists():
        raise ToolFailed(f"{EXAMPLE_CONFIG.name} is missing from the repository.")
    # The one edit the loop makes to the example: point `collector.apiKey` at
    # the file `dev up` writes the issued key into. A secret is a reference.
    text = EXAMPLE_CONFIG.read_text().replace(
        '  apiKey: ""',
        f"  apiKey: file:{KEY_FILE.relative_to(REPOSITORY_ROOT)}",
        1,
    )
    CONFIG_FILE.write_text(text)
    say(f"Wrote {CONFIG_FILE.name}. Git ignores it, so it is yours to change.")


def config_check() -> int:
    """Run `config check` against the local configuration."""
    binary = _binary("tallyowl-head")
    if not binary.exists():
        raise ToolFailed("tallyowl-head is not built. Run `./tools.sh build` first.")
    completed = subprocess.run(  # noqa: S603
        [str(binary), "--config", str(CONFIG_FILE), "config", "check"],
        cwd=str(REPOSITORY_ROOT),
        shell=False,
    )
    return completed.returncode


def _unused() -> None:  # pragma: no cover
    del sys
