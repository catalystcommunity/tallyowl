"""The cross-cluster soak: a replicated cluster, two collectors, and days of
concurrent load pointed at the one open defect.

`docs/PLAN.md` Phase 11 asks for cross-cluster collector soak tests, and L132
points them at L131: an append-log hang that reproduced at about one run in
twenty under concurrent commits and has not reproduced in 1,600 short runs
since. A soak runs that concurrency for days, which is the regime a run count
cannot reach. L145 gives the signature to watch: a commit in flight while the
durable position stands still.

What runs:

- one Corndogs, which both collectors and every head share;
- three heads. The first serves ingest, control, and queries; all three vote in
  one tablet group with `local-quorum` receipts, which is the `replicated`
  profile from `docs/DEPLOYMENT.md` section 3;
- two collectors, standing in for two clusters' collectors, both delivering to
  the first head;
- the Go soak driver from `testbed/cmd/soak`, which offers paced load and
  continuously reconciles what was acknowledged against what a query answers;
- the monitor, which watches readiness, watches the stall signature, injects
  the outage windows, and keeps a journal.

Everything is a supervised process with a recorded PID, and `soak down` stops
the whole tree and then checks every address for a survivor, because an
orphaned process holding a durable queue has already cost this project a day
(see dev.py).

The outage schedule is the exit criterion made periodic: a voter loss that
quorum survives, and a head outage that the collectors hold without losing or
duplicating anything. The kills are abrupt (`SIGKILL`), because an operator's
outage is not a graceful drain.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from urllib.request import urlopen

from .commands import REPOSITORY_ROOT, ToolFailed, require, say, warn
from .dev import _binary, _corndogs_command, _wait_for_port


def _is_running(pid: int) -> bool:
    """Running, and not a zombie.

    The monitor is the parent of everything it starts, and a child it has not
    reaped still answers signal zero as if it were alive — so the check that
    `dev.py` uses reported a killed driver as running, and the monitor never
    restarted it. The process table's own state says the truth, and finding a
    zombie reaps it so the table does not fill with them.
    """
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return False
    # The state field follows the parenthesised command name, which can itself
    # contain spaces, so split from the closing parenthesis.
    state = stat.rsplit(")", 1)[-1].split()
    if state and state[0] == "Z":
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            pass
        return False
    return True

RUN_DIR = REPOSITORY_ROOT / "run" / "soak"
DATA_DIR = REPOSITORY_ROOT / "data" / "soak"
KEY_FILE = DATA_DIR / "collector.key"
SESSION_FILE = DATA_DIR / "operator.session"
PROCESS_FILE = RUN_DIR / "processes.json"
JOURNAL_FILE = RUN_DIR / "journal.jsonl"
STATUS_FILE = RUN_DIR / "status.json"
SETTINGS_FILE = RUN_DIR / "soak.json"
MONITOR_STATE_FILE = RUN_DIR / "monitor-state.json"

#: The project the soak writes to.
PROJECT = "soak"

#: Every address the soak listens on, for the survivor check.
ADDRESSES = {
    "corndogs": "127.0.0.1:5480",
    "head-1": "127.0.0.1:5410",
    "head-1-operational": "127.0.0.1:5411",
    "head-1-replication": "127.0.0.1:5430",
    "head-2": "127.0.0.1:5510",
    "head-2-operational": "127.0.0.1:5511",
    "head-2-replication": "127.0.0.1:5530",
    "head-3": "127.0.0.1:5610",
    "head-3-operational": "127.0.0.1:5611",
    "head-3-replication": "127.0.0.1:5630",
    "collector-1": "127.0.0.1:5400",
    "collector-1-operational": "127.0.0.1:5401",
    "collector-2": "127.0.0.1:5450",
    "collector-2-operational": "127.0.0.1:5451",
}

HEADS = ("head-1", "head-2", "head-3")
COLLECTORS = ("collector-1", "collector-2")

#: The exact warning the head writes on the stall signature. See L145 and
#: crates/tallyowl-head/src/main.rs.
STALL_LINE = "This is the state L131 records"


@dataclass
class Spec:
    """How to start one process, kept so the monitor can restart it."""

    name: str
    command: list[str]
    directory: str
    environment: dict[str, str] = field(default_factory=dict)
    wait_for: str | None = None

    def to_json(self) -> dict:
        return {
            "name": self.name,
            "command": self.command,
            "directory": self.directory,
            "environment": self.environment,
            "wait_for": self.wait_for,
        }

    @staticmethod
    def from_json(raw: dict) -> "Spec":
        return Spec(
            name=raw["name"],
            command=raw["command"],
            directory=raw["directory"],
            environment=raw.get("environment", {}),
            wait_for=raw.get("wait_for"),
        )


def _read_processes() -> dict:
    if not PROCESS_FILE.exists():
        return {"processes": []}
    return json.loads(PROCESS_FILE.read_text())


def _write_processes(state: dict) -> None:
    RUN_DIR.mkdir(parents=True, exist_ok=True)
    next_file = PROCESS_FILE.with_suffix(".json.next")
    next_file.write_text(json.dumps(state, indent=2))
    next_file.replace(PROCESS_FILE)


def journal(event: str, **details) -> None:
    """One line in the soak journal. The report reads these back."""
    RUN_DIR.mkdir(parents=True, exist_ok=True)
    line = {"at_ms": int(time.time() * 1000), "event": event, **details}
    with JOURNAL_FILE.open("a") as handle:
        handle.write(json.dumps(line) + "\n")


def _start(spec: Spec) -> int:
    """Start one process in its own group, log it, and record it."""
    log = RUN_DIR / f"{spec.name}.log"
    handle = log.open("ab")
    process = subprocess.Popen(  # noqa: S603 - an array, never a shell string
        spec.command,
        start_new_session=True,
        cwd=spec.directory,
        env={**os.environ, **spec.environment},
        stdout=handle,
        stderr=subprocess.STDOUT,
        shell=False,
    )
    state = _read_processes()
    state["processes"] = [p for p in state["processes"] if p["spec"]["name"] != spec.name]
    state["processes"].append({"pid": process.pid, "spec": spec.to_json()})
    _write_processes(state)

    if spec.wait_for and not _wait_for_port(spec.wait_for, 30.0):
        warn(f"{spec.name} did not start listening on {spec.wait_for}. Its log ends with:")
        warn(log.read_text()[-2000:])
        raise ToolFailed(f"{spec.name} did not start.")
    return process.pid


def _stop_pid(pid: int, patience: float = 10.0) -> None:
    """Stop one process group: ask first, then insist."""
    try:
        os.killpg(os.getpgid(pid), signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    deadline = time.monotonic() + patience
    while time.monotonic() < deadline:
        if not _is_running(pid):
            return
        time.sleep(0.1)
    try:
        os.killpg(os.getpgid(pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def _head_config(node: int) -> dict:
    """The configuration for one head, as a flat settings tree."""
    others = [n for n in (1, 2, 3) if n != node]
    replication_port = {1: 5430, 2: 5530, 3: 5630}
    listen_port = {1: 5410, 2: 5510, 3: 5610}
    return {
        "installation": {"id": "soak", "profile": "replicated"},
        "cell": {"id": "soak-cell", "region": "soak", "controllers": 3},
        "corndogs": {
            "endpoint": ADDRESSES["corndogs"],
            "deliveryQueue": "tallyowl-soak-delivery",
            "quarantineQueue": "tallyowl-soak-quarantine",
        },
        "head": {
            "listen": f"127.0.0.1:{listen_port[node]}",
            "operationalListen": f"127.0.0.1:{listen_port[node] + 1}",
            "endpoint": ADDRESSES["head-1"],
            "dataDir": str(DATA_DIR / f"node{node}"),
        },
        "dashboard": {"enabled": False},
        "storage": {"tabletVoters": 3, "receiptPolicy": "local-quorum"},
        "replication": {
            "listen": f"127.0.0.1:{replication_port[node]}",
            "peers": ",".join(f"127.0.0.1:{replication_port[n]}" for n in others),
        },
        "node": {"failureDomain": f"domain-{node}"},
        "log": {"level": "info"},
    }


def _collector_config(index: int) -> dict:
    listen_port = {1: 5400, 2: 5450}[index]
    return {
        "installation": {"id": "soak", "profile": "replicated"},
        "cell": {"id": "soak-cell", "region": "soak", "controllers": 3},
        "corndogs": {
            "endpoint": ADDRESSES["corndogs"],
            "deliveryQueue": "tallyowl-soak-delivery",
            "quarantineQueue": "tallyowl-soak-quarantine",
        },
        "collector": {
            "listen": f"127.0.0.1:{listen_port}",
            "operationalListen": f"127.0.0.1:{listen_port + 1}",
            "apiKey": f"file:{KEY_FILE}",
            # The agreed outage window. A collector may keep using a key answer
            # it already had for this long when the head is away, so the window
            # an installation can hold is the window this setting covers. The
            # soak's head outages are minutes; an hour holds them with room.
            "keyCacheGrace": "1h",
        },
        "head": {"endpoint": ADDRESSES["head-1"]},
        "log": {"level": "info"},
    }


def _write_yaml(tree: dict, into: Path) -> None:
    """Write a nested settings tree as YAML.

    The tree holds strings, numbers, and booleans only, so this stays a small
    writer rather than a dependency.
    """

    def render(value, indent: int) -> list[str]:
        lines: list[str] = []
        for key, held in value.items():
            if isinstance(held, dict):
                lines.append(" " * indent + f"{key}:")
                lines.extend(render(held, indent + 2))
            elif isinstance(held, bool):
                lines.append(" " * indent + f"{key}: {'true' if held else 'false'}")
            else:
                lines.append(" " * indent + f"{key}: {held}")
        return lines

    into.write_text("\n".join(render(tree, 0)) + "\n")


def _provision() -> None:
    """Make the soak's key and session, before the first head starts."""
    binary = _binary("tallyowl-head")
    config = RUN_DIR / "head-1.yaml"
    if not KEY_FILE.exists() or not KEY_FILE.read_text().strip():
        say(f"Making a key for the `{PROJECT}` project.")
        completed = subprocess.run(  # noqa: S603
            [str(binary), "--config", str(config), "provision", PROJECT],
            capture_output=True,
            text=True,
            shell=False,
            cwd=str(REPOSITORY_ROOT),
        )
        credential = next(
            (line.strip() for line in completed.stdout.splitlines() if line.startswith("tow_")),
            "",
        )
        if completed.returncode != 0 or not credential:
            warn(completed.stdout)
            warn(completed.stderr)
            raise ToolFailed("The soak key could not be made.")
        KEY_FILE.write_text(credential + "\n")
        KEY_FILE.chmod(0o600)
    if not SESSION_FILE.exists() or not SESSION_FILE.read_text().strip():
        say("Signing in the soak operator.")
        completed = subprocess.run(  # noqa: S603
            [str(binary), "--config", str(config), "session", "create", "soak-operator"],
            capture_output=True,
            text=True,
            shell=False,
            cwd=str(REPOSITORY_ROOT),
        )
        token = next(
            (line.strip() for line in completed.stdout.splitlines() if line.startswith("tos_")),
            "",
        )
        if completed.returncode != 0 or not token:
            warn(completed.stdout)
            warn(completed.stderr)
            raise ToolFailed("The soak session could not be made.")
        SESSION_FILE.write_text(token + "\n")
        SESSION_FILE.chmod(0o600)


def up(
    rate: int = 500,
    voter_outage_every_s: int = 6 * 3600,
    voter_outage_window_s: int = 120,
    head_outage_every_s: int = 12 * 3600,
    head_outage_window_s: int = 600,
) -> int:
    """Start the soak: the cluster, the collectors, the driver, the monitor."""
    existing = [p for p in _read_processes()["processes"] if _is_running(p["pid"])]
    if existing:
        raise ToolFailed("A soak is already running. Run `./tools.sh soak down` first.")

    go = require(
        "go",
        "Source the catalyst toolchain first: "
        '`source "${CATALYST_TOOLS:-$HOME/.local/catalyst-tools}/env.sh"`.',
    )

    RUN_DIR.mkdir(parents=True, exist_ok=True)
    DATA_DIR.mkdir(parents=True, exist_ok=True)

    # Release binaries. A days-long measurement against a debug build measures
    # the debug build; this is L074's lesson applied before the run rather than
    # after it.
    say("Building the release binaries.")
    subprocess.run(  # noqa: S603
        ["cargo", "build", "--release", "-p", "tallyowl-head", "-p", "tallyowl-collector"],
        cwd=str(REPOSITORY_ROOT),
        check=True,
        shell=False,
    )
    say("Building the soak driver.")
    driver_binary = RUN_DIR / "bin" / "tallyowl-soak"
    driver_binary.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(  # noqa: S603
        [go, "build", "-o", str(driver_binary), "./cmd/soak"],
        cwd=str(REPOSITORY_ROOT / "testbed"),
        env={**os.environ, "GOFLAGS": "-mod=mod"},
        check=True,
        shell=False,
    )

    # The configurations, written before anything starts so a person can read
    # what the soak is running.
    for node in (1, 2, 3):
        _write_yaml(_head_config(node), RUN_DIR / f"head-{node}.yaml")
    for index in (1, 2):
        _write_yaml(_collector_config(index), RUN_DIR / f"collector-{index}.yaml")
    SETTINGS_FILE.write_text(
        json.dumps(
            {
                "rate": rate,
                "voter_outage_every_s": voter_outage_every_s,
                "voter_outage_window_s": voter_outage_window_s,
                "head_outage_every_s": head_outage_every_s,
                "head_outage_window_s": head_outage_window_s,
            },
            indent=2,
        )
    )

    head_binary = _release_or_fail("tallyowl-head")
    collector_binary = _release_or_fail("tallyowl-collector")

    corndogs_command, corndogs_dir = _corndogs_command()
    _start(
        Spec(
            name="corndogs",
            command=corndogs_command,
            directory=str(corndogs_dir),
            environment={
                "STORAGE_BACKEND": "file",
                "CORNDOGS_FILESTORE_DIR": str(DATA_DIR / "corndogs"),
                "CORNDOGS_FILESTORE_SYNC": "group",
                "CORNDOGS_LISTEN": ADDRESSES["corndogs"],
                "CORNDOGS_HTTP_LISTEN": "127.0.0.1:5481",
                "LOGLEVEL": "error",
            },
            wait_for=ADDRESSES["corndogs"],
        )
    )
    say("Corndogs is up.")

    _provision()

    for node in (1, 2, 3):
        _start(
            Spec(
                name=f"head-{node}",
                command=[str(head_binary), "--config", str(RUN_DIR / f"head-{node}.yaml")],
                directory=str(REPOSITORY_ROOT),
                wait_for=ADDRESSES[f"head-{node}"],
            )
        )
        say(f"head-{node} is up.")

    for index in (1, 2):
        _start(
            Spec(
                name=f"collector-{index}",
                command=[
                    str(collector_binary),
                    "--config",
                    str(RUN_DIR / f"collector-{index}.yaml"),
                ],
                directory=str(REPOSITORY_ROOT),
                wait_for=ADDRESSES[f"collector-{index}"],
            )
        )
        say(f"collector-{index} is up.")

    _start(
        Spec(
            name="driver",
            command=[str(driver_binary)],
            directory=str(REPOSITORY_ROOT),
            environment={
                "SOAK_COLLECTORS": f"{ADDRESSES['collector-1']},{ADDRESSES['collector-2']}",
                "SOAK_HEAD": ADDRESSES["head-1"],
                "SOAK_CREDENTIAL": KEY_FILE.read_text().strip(),
                "SOAK_SESSION": SESSION_FILE.read_text().strip(),
                "SOAK_STATUS_FILE": str(STATUS_FILE),
                "SOAK_RATE": str(rate),
            },
        )
    )
    say("The driver is offering load.")

    _start(
        Spec(
            name="monitor",
            command=[
                "uv",
                "run",
                "--project",
                str(REPOSITORY_ROOT / "tools"),
                "python",
                "-m",
                "tallyowl_tools",
                "soak",
                "monitor",
            ],
            directory=str(REPOSITORY_ROOT),
        )
    )
    say("The monitor is watching.")

    journal(
        "soak-started",
        rate=rate,
        voter_outage_every_s=voter_outage_every_s,
        head_outage_every_s=head_outage_every_s,
    )
    say("The soak is running. `./tools.sh soak status` says how it is going,")
    say("and `./tools.sh soak down` stops all of it.")
    return 0


def _release_or_fail(name: str) -> Path:
    path = REPOSITORY_ROOT / "target" / "release" / name
    if not path.exists():
        raise ToolFailed(f"{name} has no release build, and the build step above makes one.")
    return path


def down() -> int:
    """Stop the whole soak, monitor first so nothing gets restarted."""
    state = _read_processes()
    order = ["monitor", "driver", "collector-1", "collector-2", "head-1", "head-2", "head-3", "corndogs"]
    by_name = {p["spec"]["name"]: p for p in state["processes"]}
    for name in order:
        held = by_name.get(name)
        if not held or not _is_running(held["pid"]):
            continue
        say(f"Stopping {name}")
        # The driver drains on SIGTERM, so give it longer than the rest.
        _stop_pid(held["pid"], patience=30.0 if name == "driver" else 10.0)
    if PROCESS_FILE.exists():
        PROCESS_FILE.unlink()

    survivors = 0
    for name, address in ADDRESSES.items():
        host, _, port = address.rpartition(":")
        import socket

        try:
            with socket.create_connection((host, int(port)), timeout=0.25):
                warn(
                    f"Something is still listening on {address}, which is {name}'s "
                    "address. End it before starting again, or the next soak "
                    "inherits whatever this one left behind."
                )
                survivors += 1
        except OSError:
            pass
    if survivors == 0:
        say("Nothing survived. The addresses are clear.")
    journal("soak-stopped", survivors=survivors)
    return 0 if survivors == 0 else 1


def status() -> int:
    state = _read_processes()
    if not state["processes"]:
        say("No soak is running.")
    for held in state["processes"]:
        name = held["spec"]["name"]
        running = "running" if _is_running(held["pid"]) else "stopped"
        print(f"  {name:<14} {running:<9} pid {held['pid']}")
    if STATUS_FILE.exists():
        driver = json.loads(STATUS_FILE.read_text())
        hours = (driver["now_ms"] - driver["started_at_ms"]) / 3_600_000
        print(
            f"\n  {hours:.1f} hours in: {driver['captured']:,} captured, "
            f"{driver['acknowledged']:,} acknowledged, {driver['refused']:,} refused."
        )
        print(
            f"  Windows: {driver['windows_clean']}/{driver['windows_checked']} clean, "
            f"{driver['windows_pending']} pending. Exact lookups: "
            f"{driver['exact_lookups']} run, {driver['exact_lookups_wrong']} wrong."
        )
        if driver["mismatches"]:
            warn(f"  {len(driver['mismatches'])} window(s) did not reconcile. See {STATUS_FILE}.")
    stalls = _journal_events("wal-stall-warning") + _journal_events("stall-signature")
    if stalls:
        warn(f"  The stall signature has appeared {len(stalls)} time(s). See {JOURNAL_FILE}.")
    else:
        print("  The stall signature has not appeared.")
    return 0


def _journal_events(kind: str) -> list[dict]:
    if not JOURNAL_FILE.exists():
        return []
    out = []
    for line in JOURNAL_FILE.read_text().splitlines():
        try:
            held = json.loads(line)
        except json.JSONDecodeError:
            continue
        if held.get("event") == kind:
            out.append(held)
    return out


def report() -> int:
    """Everything the journal and the driver know, summarized."""
    if not JOURNAL_FILE.exists():
        say("There is no journal. Has a soak run?")
        return 1
    events = [json.loads(line) for line in JOURNAL_FILE.read_text().splitlines() if line.strip()]
    outages = [e for e in events if e["event"] in ("outage-begin", "outage-end")]
    stalls = [e for e in events if e["event"] in ("wal-stall-warning", "stall-signature")]
    exits = [e for e in events if e["event"] == "unexpected-exit"]
    print(f"Journal events: {len(events)}")
    print(f"  Outages injected and recovered: {len([e for e in outages if e['event'] == 'outage-end'])}")
    print(f"  Stall-signature sightings: {len(stalls)}")
    for stall in stalls:
        print(f"    at {stall['at_ms']}: {stall.get('detail', '')[:200]}")
    print(f"  Unexpected process exits: {len(exits)}")
    for held in exits:
        print(f"    {held.get('name')} at {held['at_ms']}")
    if STATUS_FILE.exists():
        print("\nDriver status:")
        print(STATUS_FILE.read_text())
    return 0


def roll() -> int:
    """The rolling-upgrade drill, against the running soak.

    `docs/DEPLOYMENT.md` section 7 gives the order: head ingest and query roles
    first, then storage nodes one failure domain at a time, then collectors
    last. This restarts every service in that order, gracefully, while the
    driver keeps offering load — which is what a rolling upgrade is from the
    installation's side.

    Today every node restarts into the same binary, because 0.0.0 is the only
    version there is and D31 opens the compatibility window at the first
    release candidate. The drill therefore proves the procedure — order,
    drain, quorum held, nothing refused, nothing lost — and takes a second
    version the day one exists: build the new binary, run this, and the
    reconciliation says whether the window held.
    """
    state = _read_processes()
    by_name = {p["spec"]["name"]: p for p in state["processes"]}
    order = ["head-1", "head-2", "head-3", "collector-1", "collector-2"]
    missing = [name for name in order if name not in by_name]
    if missing:
        raise ToolFailed("No soak is running. Start one with `./tools.sh soak up` first.")

    journal("roll-begin", order=order)
    for name in order:
        held = _read_processes()
        entry = next(p for p in held["processes"] if p["spec"]["name"] == name)
        spec = Spec.from_json(entry["spec"])
        say(f"Rolling {name}.")
        # Graceful: an upgrade is a voluntary disruption, and the service
        # drains. Head ingest stops readiness before it drains; a collector
        # finishes or releases its claimed tasks and leaves queued data
        # durable.
        _stop_pid(entry["pid"])
        _start(spec)
        address = ADDRESSES[name]
        if not _wait_for_port(address, 60.0):
            journal("roll-failed", name=name)
            raise ToolFailed(f"{name} did not come back. The roll stops here.")
        operational = ADDRESSES.get(f"{name}-operational")
        if operational:
            deadline = time.time() + 60
            while time.time() < deadline:
                if _ready(operational):
                    break
                time.sleep(1)
            else:
                journal("roll-failed", name=name)
                raise ToolFailed(f"{name} came back and never became ready.")
        journal("rolled", name=name)
        say(f"{name} is back and ready.")
    journal("roll-end")
    say("The roll finished. The driver's next reconciliation windows say")
    say("whether anything was lost; `./tools.sh soak status` shows them.")
    return 0


# ---------------------------------------------------------------------------
# The monitor
# ---------------------------------------------------------------------------


def _scrape(address: str) -> dict[str, float]:
    """The gauges this monitor watches, from one /metrics endpoint."""
    wanted = (
        "tallyowl_wal_commit_in_flight_count",
        "tallyowl_wal_durable_position_count",
        "tallyowl_integrity_failures_total",
    )
    out: dict[str, float] = {}
    with urlopen(f"http://{address}/metrics", timeout=5) as response:  # noqa: S310
        for raw in response.read().decode().splitlines():
            for name in wanted:
                if raw.startswith(name + " ") or raw.startswith(name + "{"):
                    parts = raw.split()
                    if len(parts) == 2:
                        out[name] = float(parts[1])
    return out


def _ready(address: str) -> bool:
    try:
        with urlopen(f"http://{address}/readyz", timeout=5) as response:  # noqa: S310
            return response.status == 200
    except Exception:
        return False


def monitor() -> int:
    """Watch, journal, and inject the outage schedule. Runs until stopped."""
    settings = json.loads(SETTINGS_FILE.read_text()) if SETTINGS_FILE.exists() else {}
    voter_every = settings.get("voter_outage_every_s", 6 * 3600)
    voter_window = settings.get("voter_outage_window_s", 120)
    head_every = settings.get("head_outage_every_s", 12 * 3600)
    head_window = settings.get("head_outage_window_s", 600)

    state = (
        json.loads(MONITOR_STATE_FILE.read_text()) if MONITOR_STATE_FILE.exists() else {}
    )
    now = time.time()
    state.setdefault("next_voter_outage", now + voter_every)
    state.setdefault("next_head_outage", now + head_every)
    state.setdefault("voter_turn", 0)
    state.setdefault("log_offsets", {})
    # The durable position each head last showed, for the stall signature.
    last_position: dict[str, tuple[float, int]] = {}

    stopping = {"now": False}

    def stop(_signum, _frame):
        stopping["now"] = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    journal("monitor-started")
    while not stopping["now"]:
        time.sleep(30)
        if stopping["now"]:
            break
        now = time.time()

        # 1. The stall signature, from the metrics the store now publishes.
        for name in HEADS:
            operational = ADDRESSES[f"{name}-operational"]
            try:
                gauges = _scrape(operational)
            except Exception:
                continue
            committing = gauges.get("tallyowl_wal_commit_in_flight_count", 0)
            position = int(gauges.get("tallyowl_wal_durable_position_count", -1))
            held = last_position.get(name)
            if committing >= 1 and held is not None and held[1] == position:
                stalled_for = now - held[0]
                if stalled_for >= 60:
                    journal(
                        "stall-signature",
                        name=name,
                        durable_position=position,
                        stalled_seconds=int(stalled_for),
                        detail=(
                            "A commit has been in flight while the durable "
                            "position stood still. This is the L145 signature; "
                            "take the stacks before anything is restarted. "
                            "L132 says how."
                        ),
                    )
            if held is None or held[1] != position:
                last_position[name] = (now, position)
            failures = gauges.get("tallyowl_integrity_failures_total", 0)
            if failures > 0:
                journal("integrity-failure", name=name, failures=failures)

        # 2. The head's own warning line, from its log.
        for name in HEADS:
            log = RUN_DIR / f"{name}.log"
            if not log.exists():
                continue
            offset = state["log_offsets"].get(name, 0)
            size = log.stat().st_size
            if size < offset:
                offset = 0
            if size > offset:
                with log.open("rb") as handle:
                    handle.seek(offset)
                    fresh = handle.read().decode(errors="replace")
                state["log_offsets"][name] = size
                for line in fresh.splitlines():
                    if STALL_LINE in line:
                        journal("wal-stall-warning", name=name, detail=line[-500:])

        # 3. Restart anything that died outside a planned outage.
        planned = set(state.get("in_outage", []))
        processes = _read_processes()
        for held in processes["processes"]:
            spec = Spec.from_json(held["spec"])
            if spec.name == "monitor" or spec.name in planned:
                continue
            if not _is_running(held["pid"]):
                journal("unexpected-exit", name=spec.name)
                try:
                    _start(spec)
                    journal("restarted", name=spec.name)
                except ToolFailed as failure:
                    journal("restart-failed", name=spec.name, detail=str(failure))

        # 4. The outage schedule.
        if now >= state["next_voter_outage"]:
            victim = ("head-2", "head-3")[state["voter_turn"] % 2]
            state["voter_turn"] += 1
            state["next_voter_outage"] = now + voter_every
            _outage(victim, voter_window, state, stopping)
        if now >= state["next_head_outage"]:
            state["next_head_outage"] = now + head_every
            _outage("head-1", head_window, state, stopping)

        MONITOR_STATE_FILE.write_text(json.dumps(state, indent=2))

    journal("monitor-stopped")
    return 0


def _outage(name: str, window_s: int, state: dict, stopping: dict) -> None:
    """Kill one head abruptly, hold the window, and bring it back.

    Abruptly, because the exit criterion is about an outage and not a drain:
    the collectors must hold the window, and nothing acknowledged may be lost
    or duplicated afterwards. The driver's reconciliation is what proves that.

    A head that showed the stall signature in the last six hours is spared:
    that process may be the only evidence L131 has ever left alive, and a
    scheduled kill would destroy exactly what a person needs to attach to.
    The first soak night proved the point the hard way — three stall episodes
    on one voter, and the schedule killed it before anybody could take a
    stack.
    """
    six_hours_ago = int((time.time() - 6 * 3600) * 1000)
    recent_stall = any(
        held.get("name") == name and held["at_ms"] > six_hours_ago
        for held in _journal_events("stall-signature")
    )
    if recent_stall:
        journal(
            "outage-skipped",
            name=name,
            reason=(
                "This head showed the stall signature recently, and killing "
                "it would destroy the evidence. Take the stacks first; L132 "
                "says how."
            ),
        )
        return
    processes = _read_processes()
    held = next((p for p in processes["processes"] if p["spec"]["name"] == name), None)
    if held is None or not _is_running(held["pid"]):
        return
    spec = Spec.from_json(held["spec"])
    state.setdefault("in_outage", []).append(name)
    MONITOR_STATE_FILE.write_text(json.dumps(state, indent=2))
    journal("outage-begin", name=name, window_s=window_s)
    try:
        os.killpg(os.getpgid(held["pid"]), signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        try:
            os.kill(held["pid"], signal.SIGKILL)
        except ProcessLookupError:
            pass

    waited = 0
    while waited < window_s and not stopping["now"]:
        time.sleep(min(5, window_s - waited))
        waited += 5

    try:
        _start(spec)
        recovered = _wait_for_port(ADDRESSES[name], 60.0)
        journal("outage-end", name=name, recovered=recovered)
    except ToolFailed as failure:
        journal("outage-end", name=name, recovered=False, detail=str(failure))
    state["in_outage"] = [n for n in state.get("in_outage", []) if n != name]
    MONITOR_STATE_FILE.write_text(json.dumps(state, indent=2))
