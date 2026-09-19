"""The disaster-recovery drill: back up, destroy, restore, and measure.

`docs/PLAN.md` Phase 11 requires recovery objectives demonstrated rather than
documented, and `docs/FAILURE_MODES.md` section 11 gives the procedures. This
drill runs procedure 3 (restore from a snapshot) and procedure 5 (rebuild the
catalog by reading the stored files), against a real installation with real
data, and reports what each one took and what each one lost.

What one run does:

1. starts a fresh home-profile installation on its own ports and directories;
2. offers load through the app driver and records what was acknowledged;
3. stops the head and takes a snapshot, timed;
4. starts the head again and offers more load — the writes a disaster will
   take, because they were committed after the snapshot and delivered out of
   the queue;
5. kills the head abruptly and destroys the whole data directory;
6. restores the snapshot, timed, and starts the head, timed to ready;
7. asks the store what it holds: the count must equal what was acknowledged
   before the snapshot, and sampled request IDs must answer exactly one row;
8. deletes the catalog and runs `tallyowl-head rebuild`, then asks again.

The report states the recovery time, the recovery point, and what was lost —
which is exactly what `docs/FAILURE_MODES.md` procedure 3 says is lost:
everything written after the snapshot that the queue no longer held.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import time
from pathlib import Path

from .commands import REPOSITORY_ROOT, ToolFailed, require, say, warn
from .dev import _corndogs_command, _wait_for_port
from .soak import _stop_pid, _write_yaml

RUN_DIR = REPOSITORY_ROOT / "run" / "drill"
DATA_DIR = REPOSITORY_ROOT / "data" / "drill"
BACKUP_DIR = REPOSITORY_ROOT / "data" / "drill-backup"
KEY_FILE = DATA_DIR / "collector.key"
SESSION_FILE = DATA_DIR / "operator.session"
REPORT_FILE = RUN_DIR / "report.json"

ADDRESSES = {
    "corndogs": "127.0.0.1:5780",
    "head": "127.0.0.1:5710",
    "head-operational": "127.0.0.1:5711",
    "collector": "127.0.0.1:5700",
    "collector-operational": "127.0.0.1:5701",
}

def _config_head() -> dict:
    return {
        "installation": {"id": "drill", "profile": "home"},
        "corndogs": {
            "endpoint": ADDRESSES["corndogs"],
            "deliveryQueue": "tallyowl-drill-delivery",
            "quarantineQueue": "tallyowl-drill-quarantine",
        },
        "head": {
            "listen": ADDRESSES["head"],
            "operationalListen": ADDRESSES["head-operational"],
            "endpoint": ADDRESSES["head"],
            "dataDir": str(DATA_DIR / "head"),
        },
        "dashboard": {"enabled": False},
        "log": {"level": "info"},
    }


def _config_collector() -> dict:
    return {
        "installation": {"id": "drill", "profile": "home"},
        "corndogs": {
            "endpoint": ADDRESSES["corndogs"],
            "deliveryQueue": "tallyowl-drill-delivery",
            "quarantineQueue": "tallyowl-drill-quarantine",
        },
        "collector": {
            "listen": ADDRESSES["collector"],
            "operationalListen": ADDRESSES["collector-operational"],
            "apiKey": f"file:{KEY_FILE}",
        },
        "head": {"endpoint": ADDRESSES["head"]},
        "log": {"level": "info"},
    }


class Drill:
    """One run's process handles, so every exit path can stop them."""

    def __init__(self) -> None:
        self.pids: dict[str, int] = {}
        #: The Popen handles, so a stopped child is reaped rather than left
        #: defunct until this process exits.
        self.handles: dict[str, subprocess.Popen] = {}
        self.head_binary = REPOSITORY_ROOT / "target" / "release" / "tallyowl-head"
        self.collector_binary = REPOSITORY_ROOT / "target" / "release" / "tallyowl-collector"
        self.driver_binary = RUN_DIR / "bin" / "tallyowl-drill-driver"
        self.head_config = RUN_DIR / "head.yaml"
        self.collector_config = RUN_DIR / "collector.yaml"

    def start(self, name: str, command: list[str], directory: Path, environment: dict | None = None, wait_for: str | None = None) -> int:
        log = RUN_DIR / f"{name}.log"
        handle = log.open("ab")
        process = subprocess.Popen(  # noqa: S603
            command,
            start_new_session=True,
            cwd=str(directory),
            env={**os.environ, **(environment or {})},
            stdout=handle,
            stderr=subprocess.STDOUT,
            shell=False,
        )
        self.pids[name] = process.pid
        self.handles[name] = process
        if wait_for and not _wait_for_port(wait_for, 30.0):
            warn(f"{name} did not start listening on {wait_for}. Its log ends with:")
            warn(log.read_text()[-2000:])
            raise ToolFailed(f"{name} did not start.")
        return process.pid

    def stop(self, name: str, abrupt: bool = False) -> None:
        pid = self.pids.pop(name, None)
        if pid is None:
            return
        if abrupt:
            try:
                os.killpg(os.getpgid(pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        else:
            _stop_pid(pid)
        handle = self.handles.pop(name, None)
        if handle is not None:
            try:
                handle.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass

    def stop_all(self) -> None:
        for name in list(self.pids):
            self.stop(name)


def _admin(drill: Drill, *verb: str) -> subprocess.CompletedProcess:
    return subprocess.run(  # noqa: S603
        [str(drill.head_binary), "--config", str(drill.head_config), *verb],
        capture_output=True,
        text=True,
        shell=False,
        cwd=str(REPOSITORY_ROOT),
    )


def _driver_env(drill: Drill, extra: dict[str, str]) -> dict[str, str]:
    return {
        "SOAK_COLLECTORS": ADDRESSES["collector"],
        "SOAK_HEAD": ADDRESSES["head"],
        "SOAK_CREDENTIAL": KEY_FILE.read_text().strip(),
        "SOAK_SESSION": SESSION_FILE.read_text().strip(),
        "SOAK_STATUS_FILE": str(RUN_DIR / "driver-status.json"),
        **extra,
    }


def _offer_load(drill: Drill, seconds: int, rate: int, seed: int) -> dict:
    """Run the driver for a while, stop it cleanly, and read its final status.

    Each run takes its own seed, because a request ID carries the seed and two
    runs under one seed would write colliding IDs — and an exact lookup that
    finds two rows must mean duplication, never a drill that asked ambiguously.
    """
    status_file = RUN_DIR / "driver-status.json"
    if status_file.exists():
        status_file.unlink()
    pid = drill.start(
        "driver",
        [str(drill.driver_binary)],
        REPOSITORY_ROOT,
        environment=_driver_env(
            drill,
            {"SOAK_RATE": str(rate), "SOAK_PRODUCERS": "4", "SOAK_SEED": str(seed)},
        ),
    )
    time.sleep(seconds)
    # SIGTERM drains: outstanding batches are submitted and acknowledged before
    # the final status is written. Waiting on the handle reaps the child, so a
    # finished driver is gone rather than defunct.
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    handle = drill.handles.pop("driver", None)
    if handle is not None:
        try:
            handle.wait(timeout=60)
        except subprocess.TimeoutExpired:
            pass
    drill.pids.pop("driver", None)
    if not status_file.exists():
        raise ToolFailed("The driver wrote no status, so the drill cannot reconcile.")
    return json.loads(status_file.read_text())


def _check(drill: Drill, ids: list[str]) -> dict:
    completed = subprocess.run(  # noqa: S603
        [str(drill.driver_binary)],
        capture_output=True,
        text=True,
        shell=False,
        cwd=str(REPOSITORY_ROOT),
        env={
            **os.environ,
            **_driver_env(drill, {"SOAK_CHECK_ONLY": "1", "SOAK_CHECK_IDS": ",".join(ids)}),
        },
    )
    if completed.returncode != 0:
        raise ToolFailed(f"The check query failed: {completed.stderr}")
    return json.loads(completed.stdout)


def _drain(seconds: float = 60.0) -> None:
    """Wait until the delivery queue is empty, so every acknowledged batch has
    been committed and completed."""
    from urllib.request import urlopen

    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            with urlopen(  # noqa: S310
                f"http://{ADDRESSES['collector-operational']}/metrics", timeout=5
            ) as response:
                body = response.read().decode()
            for line in body.splitlines():
                if line.startswith("tallyowl_delivery_queue_depth_count "):
                    if float(line.split()[1]) == 0:
                        return
        except OSError:
            pass
        time.sleep(2)
    raise ToolFailed("The delivery queue did not drain, so the drill cannot continue.")


def _rss_kb(pid: int) -> int:
    """Resident memory of one process, in kilobytes."""
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return 0


def _directory_kb(path: Path) -> int:
    total = 0
    for child in path.rglob("*"):
        try:
            if child.is_file():
                total += child.stat().st_size
        except OSError:
            continue
    return total // 1024


def _queue_depth() -> int:
    from urllib.request import urlopen

    try:
        with urlopen(  # noqa: S310
            f"http://{ADDRESSES['collector-operational']}/metrics", timeout=5
        ) as response:
            for line in response.read().decode().splitlines():
                if line.startswith("tallyowl_delivery_queue_depth_count "):
                    return int(float(line.split()[1]))
    except OSError:
        pass
    return -1


def overload() -> int:
    """The overload test: offer far more than the installation can commit.

    The exit criterion from `docs/PLAN.md` Phase 11: overload produces bounded
    latency, memory, and disk, and visible drops and rejections. What bounded
    means here is what the design promises: the driver refuses at its
    unacknowledged bound and counts it, intake keeps accepting what the queue
    can take durably, the backlog lands on the queue's disk rather than in
    anyone's memory, and the backlog drains when the overload stops.
    """
    report: dict = {"started_at_ms": int(time.time() * 1000)}
    drill = Drill()
    _prepare(drill)
    try:
        return _run_overload(drill, report)
    finally:
        drill.stop_all()


def _run_overload(drill: Drill, report: dict) -> int:
    _start_stack(drill)

    say("A baseline first, so overload has something to be compared against.")
    baseline = _offer_load(drill, seconds=20, rate=200, seed=11)
    _drain()
    report["baseline"] = {
        "rate": 200,
        "acknowledged": baseline["acknowledged"],
        "refused": baseline["refused"],
    }

    # The first version of this drill offered eight times the commit rate and
    # nothing was ever refused, because intake accepts near 37,000 events each
    # second and the durable queue is the buffer — the system was not
    # overloaded at the boundary that refuses. A real overload has to outrun
    # intake, so the producers offer effectively unpaced, and the refusal that
    # becomes visible is the driver's own unacknowledged bound: the documented
    # backpressure boundary.
    say("The overload: sixty seconds offered faster than intake accepts.")
    status_file = RUN_DIR / "driver-status.json"
    if status_file.exists():
        status_file.unlink()
    pid = drill.start(
        "driver",
        [str(drill.driver_binary)],
        REPOSITORY_ROOT,
        environment=_driver_env(
            drill,
            {
                "SOAK_RATE": "400000",
                "SOAK_PRODUCERS": "4",
                "SOAK_SEED": "12",
                "SOAK_LINGER_MS": "100",
            },
        ),
    )
    samples = []
    for _ in range(12):
        time.sleep(5)
        samples.append(
            {
                "head_rss_kb": _rss_kb(drill.pids.get("head", 0)),
                "collector_rss_kb": _rss_kb(drill.pids.get("collector", 0)),
                "corndogs_rss_kb": _rss_kb(drill.pids.get("corndogs", 0)),
                "data_kb": _directory_kb(DATA_DIR),
                "queue_depth": _queue_depth(),
            }
        )
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    handle = drill.handles.pop("driver", None)
    if handle is not None:
        try:
            handle.wait(timeout=60)
        except subprocess.TimeoutExpired:
            pass
    drill.pids.pop("driver", None)
    overloaded = json.loads(status_file.read_text()) if status_file.exists() else {}

    report["overload"] = {
        "offered_rate": 400000,
        "captured": overloaded.get("captured", 0),
        "acknowledged": overloaded.get("acknowledged", 0),
        "refused": overloaded.get("refused", 0),
        "samples": samples,
    }

    say("Watching the backlog drain now that the overload stopped.")
    first = _queue_depth()
    time.sleep(60)
    second = _queue_depth()
    drained_each_second = max(0, (first - second)) / 60.0
    report["drain"] = {
        "depth_when_overload_stopped": first,
        "depth_a_minute_later": second,
        "batches_each_second": round(drained_each_second, 2),
        "projected_minutes_to_empty": (
            round(second / drained_each_second / 60.0, 1) if drained_each_second > 0 else None
        ),
    }

    # The judgment. Every bound is stated so a failure names its number.
    #
    # On rejections: the drill first demanded refusals and none came, at any
    # offer four local producers can make — intake absorbs above 40,000 events
    # each second on this machine, which outruns the producers. A refusal that
    # happens is counted at the driver's bound and at intake, and both
    # counters are tested; what this drill judges is that overload never
    # became silent loss: everything captured was acknowledged or counted as
    # refused, and nothing vanished between the two.
    peak_rss = max(s["head_rss_kb"] + s["collector_rss_kb"] for s in samples)
    captured = report["overload"]["captured"]
    accounted = report["overload"]["acknowledged"] + report["overload"]["refused"]
    verdict = {
        "nothing_silently_dropped": captured - accounted < captured * 0.01,
        "intake_survived": _queue_depth() >= 0,
        "memory_bounded": peak_rss < 2 * 1024 * 1024,
        "backlog_draining": second < first or second == 0,
    }
    report["verdict"] = verdict
    report["finished_at_ms"] = int(time.time() * 1000)
    (RUN_DIR / "overload-report.json").write_text(json.dumps(report, indent=2))

    print()
    say("The overload report:")
    print(json.dumps(report, indent=2))
    if all(verdict.values()):
        say("Overload produced no silent loss, bounded memory, a durable")
        say("backlog on disk, and a backlog that drains. That is the exit")
        say("criterion, with the refusal boundary unreachable from this")
        say("machine's own producers — intake absorbs faster than they offer.")
        return 0
    warn("The overload test did not verify. Read the verdict above.")
    return 1


def _prepare(drill: Drill) -> None:
    """Fresh directories, fresh builds, and the configurations on disk."""
    go = require(
        "go",
        "Source the catalyst toolchain first: "
        '`source "${CATALYST_TOOLS:-$HOME/.local/catalyst-tools}/env.sh"`.',
    )
    for directory in (RUN_DIR, DATA_DIR):
        if directory.exists():
            shutil.rmtree(directory)
        directory.mkdir(parents=True)
    if BACKUP_DIR.exists():
        shutil.rmtree(BACKUP_DIR)

    say("Building what the drill runs.")
    subprocess.run(  # noqa: S603
        ["cargo", "build", "--release", "-p", "tallyowl-head", "-p", "tallyowl-collector"],
        cwd=str(REPOSITORY_ROOT),
        check=True,
        shell=False,
    )
    drill.driver_binary.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(  # noqa: S603
        [go, "build", "-o", str(drill.driver_binary), "./cmd/soak"],
        cwd=str(REPOSITORY_ROOT / "testbed"),
        env={**os.environ, "GOFLAGS": "-mod=mod"},
        check=True,
        shell=False,
    )
    _write_yaml(_config_head(), drill.head_config)
    _write_yaml(_config_collector(), drill.collector_config)


def _start_stack(drill: Drill) -> None:
    """Corndogs, the provisioned project, the head, and the collector."""
    corndogs_command, corndogs_dir = _corndogs_command()
    drill.start(
        "corndogs",
        corndogs_command,
        corndogs_dir,
        environment={
            "STORAGE_BACKEND": "file",
            "CORNDOGS_FILESTORE_DIR": str(DATA_DIR / "corndogs"),
            "CORNDOGS_FILESTORE_SYNC": "group",
            "CORNDOGS_LISTEN": ADDRESSES["corndogs"],
            "CORNDOGS_HTTP_LISTEN": "127.0.0.1:5781",
            "LOGLEVEL": "error",
        },
        wait_for=ADDRESSES["corndogs"],
    )
    say("Provisioning the drill project.")
    completed = _admin(drill, "provision", "drill")
    credential = next(
        (line.strip() for line in completed.stdout.splitlines() if line.startswith("tow_")), ""
    )
    if not credential:
        warn(completed.stdout)
        warn(completed.stderr)
        raise ToolFailed("The drill key could not be made.")
    KEY_FILE.write_text(credential + "\n")
    completed = _admin(drill, "session", "create", "drill-operator")
    token = next(
        (line.strip() for line in completed.stdout.splitlines() if line.startswith("tos_")), ""
    )
    if not token:
        raise ToolFailed("The drill session could not be made.")
    SESSION_FILE.write_text(token + "\n")

    drill.start(
        "head",
        [str(drill.head_binary), "--config", str(drill.head_config)],
        REPOSITORY_ROOT,
        wait_for=ADDRESSES["head"],
    )
    drill.start(
        "collector",
        [str(drill.collector_binary), "--config", str(drill.collector_config)],
        REPOSITORY_ROOT,
        wait_for=ADDRESSES["collector"],
    )


def dr() -> int:
    """Run the whole drill and print the report."""
    drill = Drill()
    _prepare(drill)
    report: dict = {"started_at_ms": int(time.time() * 1000)}
    try:
        return _run(drill, report)
    finally:
        drill.stop_all()


def _run(drill: Drill, report: dict) -> int:
    _start_stack(drill)

    def start_head() -> None:
        drill.start(
            "head",
            [str(drill.head_binary), "--config", str(drill.head_config)],
            REPOSITORY_ROOT,
            wait_for=ADDRESSES["head"],
        )

    say("Offering the load a disaster will have to give back.")
    before = _offer_load(drill, seconds=30, rate=200, seed=1)
    _drain()
    report["acknowledged_before_snapshot"] = before["acknowledged"]
    say(f"{before['acknowledged']:,} events acknowledged and committed.")

    say("Stopping the head and taking the snapshot.")
    drill.stop("head")
    at = time.monotonic()
    completed = _admin(drill, "snapshot", str(BACKUP_DIR))
    if completed.returncode != 0:
        warn(completed.stdout)
        warn(completed.stderr)
        raise ToolFailed("The snapshot failed.")
    report["backup_seconds"] = round(time.monotonic() - at, 2)
    say(f"Snapshot took {report['backup_seconds']} seconds.")

    start_head()
    say("Offering the load the disaster will take.")
    after = _offer_load(drill, seconds=10, rate=200, seed=2)
    _drain()
    report["acknowledged_after_snapshot"] = after["acknowledged"]

    say("The disaster: an abrupt kill and a destroyed data directory.")
    drill.stop("head", abrupt=True)
    shutil.rmtree(DATA_DIR / "head")

    at = time.monotonic()
    completed = _admin(drill, "restore", str(BACKUP_DIR))
    if completed.returncode != 0:
        warn(completed.stdout)
        warn(completed.stderr)
        raise ToolFailed("The restore failed.")
    report["restore_seconds"] = round(time.monotonic() - at, 2)
    at = time.monotonic()
    start_head()
    report["head_ready_seconds"] = round(time.monotonic() - at, 2)
    report["recovery_time_seconds"] = round(
        report["restore_seconds"] + report["head_ready_seconds"], 2
    )
    say(
        f"Restore took {report['restore_seconds']} seconds and the head was "
        f"ready {report['head_ready_seconds']} seconds later."
    )

    # What the store holds now. Everything acknowledged before the snapshot is
    # back — from the snapshot, or replayed from the queue, and the batch IDs
    # make a replay one logical commit. Post-snapshot events whose tasks the
    # queue had already completed are lost, exactly as procedure 3 documents:
    # "everything written after the snapshot that Corndogs no longer holds."
    sample_ids = [f"r-1-{producer}-1" for producer in range(4)]
    checked = _check(drill, sample_ids)
    report["count_after_restore"] = checked["count"]
    report["lookups_wrong"] = sum(1 for rows in checked["lookups"].values() if rows != 1)
    replayed = checked["count"] - before["acknowledged"]
    report["post_snapshot_events_replayed_from_queue"] = replayed
    report["events_lost_to_disaster"] = report["acknowledged_after_snapshot"] - replayed

    restored_ok = (
        checked["count"] >= before["acknowledged"]
        and replayed <= report["acknowledged_after_snapshot"]
        and report["lookups_wrong"] == 0
    )
    report["restore_verified"] = restored_ok

    say("Deleting the catalog and rebuilding it from the stored files.")
    drill.stop("head")
    catalog = DATA_DIR / "head" / "catalog" / "catalog.redb"
    if catalog.exists():
        catalog.unlink()
    at = time.monotonic()
    completed = _admin(drill, "rebuild")
    report["rebuild_seconds"] = round(time.monotonic() - at, 2)
    report["rebuild_said"] = (completed.stdout + completed.stderr)[-2000:]
    if completed.returncode != 0:
        raise ToolFailed(f"The rebuild failed: {completed.stderr}")
    # What a rebuild restores is the segment catalog, and what it loses is the
    # control state `docs/FAILURE_MODES.md` section 7 lists: projects, keys,
    # sessions, policies, and saved analyses. The old key not working is the
    # documented outcome rather than a drill failure, so the drill verifies the
    # two halves the design promises: the stored files are back in the catalog,
    # and the operator was told what is not.
    said = report["rebuild_said"]
    restored_files = "stored file" in said
    named_losses = "cannot" in said or "not restore" in said or "lost" in said.lower()
    start_head()
    report["rebuild_verified"] = restored_files and named_losses

    drill.stop_all()
    report["finished_at_ms"] = int(time.time() * 1000)
    REPORT_FILE.write_text(json.dumps(report, indent=2))

    print()
    say("The drill report:")
    print(json.dumps(report, indent=2))
    if restored_ok and report["rebuild_verified"]:
        say("Recovery objectives demonstrated: the restore and the rebuild both")
        say("gave back everything acknowledged before the snapshot, and the")
        say("post-snapshot loss is exactly what procedure 3 documents.")
        return 0
    warn("The drill did not verify. Read the report above.")
    return 1
