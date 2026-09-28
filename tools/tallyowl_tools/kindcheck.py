"""`kind-check`: install both charts in a disposable cluster and prove them.

`helm-check` renders the charts and runs each service's own `config check` on
the result. It cannot see what only happens in a cluster: a verb the binary
does not route, a variable the kubelet injects, a head that cannot reach its
queue, three pods that do not agree on one membership. The first run of this
procedure by hand found four such defects that every other check had passed.
See IMPLEMENTATION_LOG.md L192.

The procedure follows DEPLOYMENT.md section 3a and section 7c as written:

1. build the service image from this tree, and make a kind cluster of one
   control plane and three workers in three zones;
2. make the authority with `tallyowl-head ca create`, and the Corndogs and
   collector certificates with the documented `openssl` commands;
3. install the head with its Corndogs sidecar, make a project key and a role
   token with the maintenance Job, and install the collector;
4. prove the data path: an application sends over TLS, the collector enrolls,
   takes the batch into Corndogs over TLS, and the head commits it;
5. prove a refusal: an application that trusts another authority is refused,
   the collector counts it, and no batch crosses;
6. prove a rotation: a replaced certificate Secret is served with no restart;
7. prove a cell: a shared Corndogs from the Corndogs chart, three heads that
   form their groups and become ready, and a leader that fails over.

This is a live environment, so the waits here are real time, each with a
bound. The owner's rule keeps real-time waits out of the unit tests and allows
them here.

The cluster gets its own kubeconfig and never touches the caller's. It is
deleted at the end unless `--keep` is given, and the temporary directory with
its keys is always removed.
"""

from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterator

from . import deps, helm, release
from .commands import REPOSITORY_ROOT, ToolFailed, run, say, warn

CLUSTER = "tallyowl-check"
IMAGE_REPOSITORY = "localhost/tallyowl"
IMAGE_TAG = "kind-check"
HOME = "tallyowl"
CELL = "cell"
EVENTS = 25

#: The Corndogs chart for a shared Corndogs. It is not in a chart registry; its
#: GitHub release attaches the package, and the digest is pinned in `deps.py`.
CORNDOGS_CHART_VERSION = "0.5.7"
CORNDOGS_CHART_URL = (
    "https://github.com/catalystcommunity/corndogs/releases/download/"
    f"helm_chart%2Fv{CORNDOGS_CHART_VERSION}/corndogs-{CORNDOGS_CHART_VERSION}.tgz"
)

KIND_CONFIG = """\
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
    labels: {topology.kubernetes.io/zone: zone-a}
  - role: worker
    labels: {topology.kubernetes.io/zone: zone-b}
  - role: worker
    labels: {topology.kubernetes.io/zone: zone-c}
"""


# ---------------------------------------------------------------------------
# Pure pieces. `tools/tests/test_kindcheck.py` covers each one.
# ---------------------------------------------------------------------------


def parse_metrics(text: str) -> dict[str, float]:
    """The samples of a Prometheus exposition, keyed by name and labels."""
    samples: dict[str, float] = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        name, _, value = line.rpartition(" ")
        try:
            samples[name] = float(value)
        except ValueError:
            continue
    return samples


def helm_list(values: list[str]) -> str:
    """A list for `--set`. Helm splits on commas, so a comma in a value is escaped."""
    return "{" + ",".join(value.replace(",", "\\,") for value in values) + "}"


def credential(logs: str, prefix: str, what: str) -> str:
    """The one-time credential a maintenance Job printed."""
    found = re.search(rf"\b{prefix}[A-Za-z0-9_-]+", logs)
    if not found:
        raise ToolFailed(f"The maintenance Job printed no {what}. Its log:\n{logs[-2000:]}")
    return found.group(0)


def corndogs_image(values: str) -> str:
    """The Corndogs image the head chart pins, which is the one to load."""
    found = re.search(r"(?m)^  image: (\S+/corndogs:\S+)$", values)
    if not found:
        raise ToolFailed("charts/tallyowl/values.yaml names no Corndogs image under corndogsDeployment.")
    return found.group(1)


@dataclass
class GroupView:
    """What one head's consensus gauges say."""

    pod: str
    groups: float
    led: float
    leaderless: float
    lag: float


def consensus_verdict(views: list[GroupView], groups: int) -> str | None:
    """None when every head runs every group and one head leads them, else why not."""
    for view in views:
        if view.groups != groups:
            return f"{view.pod} runs {view.groups:g} groups, and a cell runs {groups}."
        if view.leaderless:
            return f"{view.pod} has {view.leaderless:g} groups with no leader."
        if view.lag:
            return f"{view.pod} reports a replication lag of {view.lag:g} entries."
    leaders = [view.pod for view in views if view.led]
    if sum(view.led for view in views) != groups or len(leaders) != 1:
        return f"The groups are led by {leaders or 'no head'}, and one head should lead all {groups}."
    return None


# ---------------------------------------------------------------------------
# The cluster
# ---------------------------------------------------------------------------


def wait_until(what: str, probe: Callable[[], bool], timeout: float, interval: float = 3.0) -> None:
    """Probe until it says yes, or fail after `timeout` seconds and say what was waited for."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            if probe():
                return
        except ToolFailed:
            pass
        if time.monotonic() >= deadline:
            raise ToolFailed(f"{what} did not happen within {timeout:.0f} seconds.")
        time.sleep(interval)


def _free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class Cluster:
    """kubectl and helm, pointed at the disposable cluster's own kubeconfig."""

    def __init__(self, workdir: Path, tool: str) -> None:
        self.kubeconfig = workdir / "kubeconfig"
        self.kubectl = str(deps.fetch_kubectl())
        self.helm = deps.helm_program()
        self.kind = str(deps.fetch_kind())
        self.tool = tool
        # kind runs the container tool by name, and the job's Docker client is
        # in `.deps/bin`, which is not on the path.
        self.env = {"PATH": f"{deps.DEPENDENCY_DIR / 'bin'}{os.pathsep}{os.environ.get('PATH', '')}"}
        if Path(tool).name == "podman":
            self.env["KIND_EXPERIMENTAL_PROVIDER"] = "podman"

    def kube(self, namespace: str | None, *args: str, capture: bool = True, check: bool = True):
        command = [self.kubectl, "--kubeconfig", str(self.kubeconfig)]
        if namespace:
            command += ["--namespace", namespace]
        return run([*command, *args], capture=capture, check=check, quiet=capture)

    def helm_run(self, *args: str) -> None:
        run([self.helm, "--kubeconfig", str(self.kubeconfig), *args], capture=True, quiet=False)

    def kind_run(self, *args: str, check: bool = True):
        return run([self.kind, *args], env=self.env, check=check, capture=True, quiet=False)

    def logs(self, namespace: str, target: str, container: str | None = None) -> str:
        args = ["logs", target] + (["-c", container] if container else [])
        return self.kube(namespace, *args, check=False).stdout

    def apply_secret(self, namespace: str, name: str, files: dict[str, Path], kind: str = "generic",
                     secret_type: str | None = None) -> None:
        """Create or replace a Secret, so a rotation uses the same call."""
        args = ["create", "secret", kind, name, "--dry-run=client", "--output", "yaml"]
        if secret_type:
            args.append(f"--type={secret_type}")
        if kind == "tls":
            args += [f"--cert={files['tls.crt']}", f"--key={files['tls.key']}"]
        else:
            args += [f"--from-file={key}={path}" for key, path in files.items()]
        manifest = self.kube(namespace, *args).stdout
        target = self.kubeconfig.parent / f"secret-{namespace}-{name}.yaml"
        target.write_text(manifest)
        try:
            self.kube(namespace, "apply", "--filename", str(target))
        finally:
            target.unlink(missing_ok=True)

    @contextmanager
    def forward(self, namespace: str, target: str, remote: int) -> Iterator[int]:
        """Forward one port of a pod or a workload, for as long as the block runs."""
        local = _free_port()
        process = subprocess.Popen(  # noqa: S603 - an array, never a shell string
            [self.kubectl, "--kubeconfig", str(self.kubeconfig), "--namespace", namespace,
             "port-forward", target, f"{local}:{remote}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            def listening() -> bool:
                with socket.socket() as probe:
                    probe.settimeout(0.5)
                    return probe.connect_ex(("127.0.0.1", local)) == 0

            wait_until(f"The port forward to {target}", listening, timeout=20, interval=0.3)
            yield local
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()

    def metrics(self, namespace: str, target: str, port: int) -> dict[str, float]:
        with self.forward(namespace, target, port) as local:
            with urllib.request.urlopen(f"http://127.0.0.1:{local}/metrics", timeout=5) as response:  # noqa: S310
                return parse_metrics(response.read().decode())

    def ready(self, namespace: str, target: str, port: int) -> dict:
        with self.forward(namespace, target, port) as local:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{local}/readyz", timeout=5) as response:  # noqa: S310
                    return json.loads(response.read())
            except urllib.error.HTTPError as refused:
                return json.loads(refused.read())

    def diagnose(self) -> None:
        """Say what the cluster looked like when a step failed."""
        warn("The cluster when the step failed:")
        print(self.kube(None, "get", "pods", "--all-namespaces", "--output", "wide", check=False).stdout)
        for namespace in (HOME, CELL):
            listing = self.kube(namespace, "get", "pods", "--output", "json", check=False).stdout
            for pod in json.loads(listing or '{"items": []}').get("items", []):
                statuses = pod.get("status", {}).get("containerStatuses") or []
                if statuses and all(status.get("ready") for status in statuses):
                    continue
                name = pod["metadata"]["name"]
                warn(f"{namespace}/{name} is not ready.")
                for container in pod["spec"]["containers"]:
                    tail = self.kube(namespace, "logs", name, "-c", container["name"], "--tail", "30",
                                     check=False).stdout
                    print(f"--- {namespace}/{name} {container['name']} (last 30 lines)\n{tail}")


# ---------------------------------------------------------------------------
# The certificates, by the documented commands
# ---------------------------------------------------------------------------


def _openssl() -> str:
    from .commands import require

    program = require("openssl", "Install OpenSSL 3 or later; the documented certificate commands need it.")
    version = run([program, "version"], capture=True, quiet=True).stdout
    found = re.search(r"OpenSSL (\d+)", version)
    if not found or int(found.group(1)) < 3:
        raise ToolFailed(f"`openssl` is `{version.strip()}`, and `-copy_extensions` needs OpenSSL 3 or later.")
    return program


def leaf(authority: Path, into: Path, name: str, dns: list[str], days: int) -> dict[str, Path]:
    """A server certificate the intermediate signs, with the chain, per DEPLOYMENT.md 3a."""
    openssl = _openssl()
    key, request, certificate = into / f"{name}.key", into / f"{name}.csr", into / f"{name}.crt"
    run([openssl, "req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
         "-keyout", key, "-out", request, "-subj", f"/CN={name}",
         "-addext", "subjectAltName=" + ",".join(f"DNS:{entry}" for entry in dns)], capture=True, quiet=True)
    run([openssl, "x509", "-req", "-in", request, "-days", str(days), "-copy_extensions", "copy",
         "-CA", authority / "intermediate.crt", "-CAkey", authority / "intermediate.key",
         "-out", certificate], capture=True, quiet=True)
    certificate.write_text(certificate.read_text() + (authority / "intermediate.crt").read_text())
    request.unlink()
    return {"tls.crt": certificate, "tls.key": key}


def stranger_root(into: Path) -> Path:
    """An authority nothing in the cluster trusts."""
    root = into / "stranger.crt"
    run([_openssl(), "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
         "-keyout", into / "stranger.key", "-out", root, "-days", "1", "-subj", "/CN=stranger"],
        capture=True, quiet=True)
    return root


# ---------------------------------------------------------------------------
# The procedure
# ---------------------------------------------------------------------------


class Check:
    def __init__(self, workdir: Path, keep: bool) -> None:
        self.workdir = workdir
        self.keep = keep
        self.tool = release.container_tool()
        self.cluster = Cluster(workdir, self.tool)
        self.authority = workdir / "authority"
        self.image = [
            "--set", f"image.repository={IMAGE_REPOSITORY}",
            "--set", f"image.tag={IMAGE_TAG}",
            "--set", "image.pullPolicy=Never",
            "--set", "corndogsDeployment.imagePullPolicy=IfNotPresent",
        ]
        self.passed: list[str] = []

    # --- setup ------------------------------------------------------------

    def build(self) -> None:
        say("Building the service image from this tree. The Rust workspace compiles inside it.")
        run([self.tool, "build", "--tag", f"{IMAGE_REPOSITORY}:{IMAGE_TAG}", "--file", "Containerfile", "."],
            cwd=REPOSITORY_ROOT)
        say("Building the application that sends over TLS.")
        env = {}
        gopath = run(["go", "env", "GOPATH"], capture=True, quiet=True).stdout.strip()
        if gopath and not os.access(gopath.split(os.pathsep)[0], os.W_OK):
            # A job runs unprivileged and GOPATH belongs to root there.
            env = {"GOPATH": str(self.workdir / "go"), "GOCACHE": str(self.workdir / "go-cache")}
        self.sender = self.workdir / "tls-send"
        run(["go", "build", "-o", self.sender, "./cmd/tls-send"], cwd=REPOSITORY_ROOT / "testbed", env=env)

    def create_cluster(self) -> None:
        cluster = self.cluster
        existing = cluster.kind_run("get", "clusters", check=False).stdout.split()
        if CLUSTER in existing:
            say(f"Deleting the cluster `{CLUSTER}` a previous run left.")
            cluster.kind_run("delete", "cluster", "--name", CLUSTER)
        config = self.workdir / "kind.yaml"
        config.write_text(KIND_CONFIG)
        say(f"Making the cluster `{CLUSTER}`: one control plane and three workers in three zones.")
        cluster.kind_run("create", "cluster", "--name", CLUSTER, "--config", str(config),
                         "--kubeconfig", str(cluster.kubeconfig), "--wait", "300s")
        self.load(f"{IMAGE_REPOSITORY}:{IMAGE_TAG}")
        corndogs = corndogs_image((REPOSITORY_ROOT / "charts/tallyowl/values.yaml").read_text())
        run([self.tool, "pull", corndogs], capture=True, quiet=False)
        self.load(corndogs)

    def load(self, image: str) -> None:
        """Load an image into every node, from an archive, which works for docker and podman alike."""
        archive = self.workdir / "image.tar"
        run([self.tool, "save", "--output", archive, image], capture=True, quiet=False)
        try:
            self.cluster.kind_run("load", "image-archive", str(archive), "--name", CLUSTER)
        finally:
            archive.unlink(missing_ok=True)

    def certificates(self) -> None:
        say("Making the authority with `tallyowl-head ca create`.")
        head_binary, _ = helm._service_binaries()
        run([head_binary, "ca", "create", self.authority], capture=True, quiet=False)
        self.root = self.authority / "root.crt"

    # --- the home profile ---------------------------------------------------

    def admin(self, *words: str) -> str:
        """Run one administration verb in the maintenance Job, and return its log."""
        cluster = self.cluster
        cluster.helm_run("upgrade", HOME, str(helm.HEAD_CHART), "--namespace", HOME, "--reuse-values",
                         "--set", "deployment.maintenance.enabled=true",
                         "--set", f"deployment.maintenance.args={helm_list(list(words))}")
        selector = "tallyowl.io/component=maintenance"
        wait_until(f"The maintenance Job for `{' '.join(words[:2])}`",
                   lambda: cluster.kube(HOME, "wait", "--for=condition=complete", "job",
                                        "--selector", selector, "--timeout=10s", check=False).ok,
                   timeout=300)
        logs = cluster.kube(HOME, "logs", "--selector", selector, "--tail", "200").stdout
        cluster.helm_run("upgrade", HOME, str(helm.HEAD_CHART), "--namespace", HOME, "--reuse-values",
                         "--set", "deployment.maintenance.enabled=false")
        cluster.kube(HOME, "rollout", "status", "statefulset/tallyowl", "--timeout=300s")
        return logs

    def install_home(self) -> None:
        cluster = self.cluster
        cluster.kube(None, "create", "namespace", HOME)
        corndogs_tls = leaf(self.authority, self.workdir, "tallyowl-corndogs",
                            ["tallyowl-corndogs", f"tallyowl-corndogs.{HOME}.svc"], 90)
        self.collector_tls = leaf(self.authority, self.workdir, "tallyowl-collector",
                                  ["tallyowl-collector", f"tallyowl-collector.{HOME}.svc"], 90)
        cluster.apply_secret(HOME, "tallyowl-signing",
                             {"tls.crt": self.authority / "intermediate.crt",
                              "tls.key": self.authority / "intermediate.key"}, kind="tls")
        cluster.apply_secret(HOME, "tallyowl-authority", {"ca.crt": self.root})
        cluster.apply_secret(HOME, "tallyowl-corndogs-tls", corndogs_tls, kind="tls")
        cluster.apply_secret(HOME, "tallyowl-collector-tls", self.collector_tls, kind="tls")

        say("Installing the head with its Corndogs sidecar (DEPLOYMENT.md section 3a, step 2).")
        cluster.helm_run("install", HOME, str(helm.HEAD_CHART), "--namespace", HOME, *self.image,
                         "--set", "corndogsDeployment.enabled=true",
                         "--set", "corndogsDeployment.tlsSecret=tallyowl-corndogs-tls",
                         "--set", "deployment.networkPolicy.enabled=true",
                         "--set", "deployment.tls.signingSecret=tallyowl-signing",
                         "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
                         # kind has no Gateway API.
                         "--set", "dashboard.allowPlaintext=true")
        cluster.kube(HOME, "rollout", "status", "statefulset/tallyowl", "--timeout=300s")

        say("Making a project key and a role token with the maintenance Job (steps 3 and 7).")
        key = credential(self.admin("provision", "shop"), "tow_", "project key")
        self.key_file = self.workdir / "app.key"
        self.key_file.write_text(key)
        token = credential(self.admin("token", "create", "collectors",
                                      "collector-intake,collector-forwarder", "default"),
                           "towr_", "role token")
        cluster.apply_secret(HOME, "tallyowl-key", {"api-key": self._file("key", key)})
        cluster.apply_secret(HOME, "tallyowl-collector-token", {"token": self._file("token", token)})

        say("Installing the collector (step 8).")
        cluster.helm_run("install", "tallyowl-collector", str(helm.COLLECTOR_CHART), "--namespace", HOME,
                         *self.image,
                         "--set", "corndogs.endpoint=tallyowl-corndogs:5080",
                         "--set", "head.endpoint=tallyowl:5110",
                         "--set", "deployment.tls.certificateSecrets[0]=tallyowl-collector-tls",
                         "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
                         "--set", "deployment.tls.roleTokenSecret.name=tallyowl-collector-token",
                         "--set", "deployment.apiKeySecret.name=tallyowl-key",
                         "--set", "deployment.apiKeySecret.key=api-key")
        cluster.kube(HOME, "rollout", "status", "deployment/tallyowl-collector", "--timeout=300s")

    def _file(self, name: str, content: str) -> Path:
        path = self.workdir / f"{name}.secret"
        path.write_text(content)
        return path

    # --- the proofs ---------------------------------------------------------

    def send(self, root: Path, count: int = EVENTS) -> dict:
        with self.cluster.forward(HOME, "deployment/tallyowl-collector", 5100) as local:
            out = run([self.sender, "-address", f"127.0.0.1:{local}", "-key-file", self.key_file,
                       "-root", root, "-server-name", f"tallyowl-collector.{HOME}.svc",
                       "-count", str(count)], capture=True, quiet=True)
        return json.loads(out.stdout.strip().splitlines()[-1])

    def commits(self) -> int:
        return self.cluster.logs(HOME, "tallyowl-0", "head").count("Committed a batch")

    def collector_metrics(self) -> dict[str, float]:
        return self.cluster.metrics(HOME, "deployment/tallyowl-collector", 5101)

    def prove_data_path(self) -> None:
        cluster = self.cluster
        wait_until("The collector's enrollment with the head",
                   lambda: "Enrolled a node" in cluster.logs(HOME, "tallyowl-0", "head"), timeout=120)
        collector_log = cluster.logs(HOME, "deployment/tallyowl-collector")
        if not re.search(r"Reached the durable store.*\"transport\":\"tls\"", collector_log):
            raise ToolFailed("The collector did not report reaching Corndogs over TLS.")
        wait_until("The collector reporting ready",
                   lambda: cluster.ready(HOME, "deployment/tallyowl-collector", 5101).get("ready") is True,
                   timeout=120)
        before = self.commits()
        result = self.send(self.root)
        if result["accepted"] != EVENTS or result["lost"] or result["left_at_shutdown"]:
            raise ToolFailed(f"The application sent {EVENTS} events over TLS, and the driver saw {result}.")
        wait_until("The head committing the batch", lambda: self.commits() > before, timeout=60)
        self.passed.append(f"{EVENTS} events over TLS, into Corndogs over TLS, committed by the head")

    def prove_refusal(self) -> None:
        name = 'tallyowl_tls_handshakes_refused_total{listener="intake"}'
        before = self.collector_metrics().get(name, 0)
        result = self.send(stranger_root(self.workdir), count=3)
        told = " ".join(result["errors"] + [result.get("flush_error", "")])
        if result["accepted"] or "no trusted authority" not in told:
            raise ToolFailed(f"An application that trusts another authority was not refused as it should be: {result}")
        after = self.collector_metrics().get(name, 0)
        if after <= before:
            raise ToolFailed(f"The collector did not count the refused handshakes ({before:g} before, {after:g} after).")
        self.passed.append(f"a foreign authority refused, and counted ({after - before:g} handshakes)")

    def prove_rotation(self) -> None:
        cluster = self.cluster
        pod = cluster.kube(HOME, "get", "pods", "--selector", "app.kubernetes.io/name=tallyowl-collector",
                           "--output", "jsonpath={.items[0].metadata.name}").stdout.strip()
        restarts = cluster.kube(HOME, "get", "pod", pod, "--output",
                                "jsonpath={.status.containerStatuses[0].restartCount}").stdout.strip()
        rotated = leaf(self.authority, self.workdir, "tallyowl-collector-next",
                       ["tallyowl-collector", f"tallyowl-collector.{HOME}.svc"], 30)
        cluster.apply_secret(HOME, "tallyowl-collector-tls", rotated, kind="tls")
        gauge = 'tallyowl_tls_certificate_expiry_seconds{listener="intake"}'
        wait_until("The collector serving the replaced certificate",
                   lambda: self.collector_metrics().get(gauge, 1e12) < 31 * 86400, timeout=300, interval=10)
        now = cluster.kube(HOME, "get", "pod", pod, "--output",
                           "jsonpath={.status.containerStatuses[0].restartCount}", check=False).stdout.strip()
        if now != restarts:
            raise ToolFailed(f"The collector restarted to take the new certificate ({restarts} to {now} restarts).")
        before = self.commits()
        result = self.send(self.root)
        if result["accepted"] != EVENTS:
            raise ToolFailed(f"After the rotation the driver saw {result}.")
        wait_until("The head committing the batch after the rotation", lambda: self.commits() > before, timeout=60)
        self.passed.append("a replaced certificate served with no restart, and used")

    # --- the cell -----------------------------------------------------------

    def install_cell(self) -> None:
        cluster = self.cluster
        cluster.kube(None, "create", "namespace", CELL)
        cluster.apply_secret(CELL, "tallyowl-signing",
                             {"tls.crt": self.authority / "intermediate.crt",
                              "tls.key": self.authority / "intermediate.key"}, kind="tls")
        cluster.apply_secret(CELL, "tallyowl-authority", {"ca.crt": self.root})
        corndogs = leaf(self.authority, self.workdir, "corndogs", ["corndogs", f"corndogs.{CELL}.svc"], 90)
        cluster.apply_secret(CELL, "corndogs-tls", {**corndogs, "ca.crt": self.root},
                             secret_type="kubernetes.io/tls")

        say(f"Installing a shared Corndogs from its chart {CORNDOGS_CHART_VERSION} (DEPLOYMENT.md section 7c).")
        chart = deps._download(CORNDOGS_CHART_URL, self.workdir / "corndogs.tgz",
                               f"the Corndogs chart {CORNDOGS_CHART_VERSION}")
        cluster.helm_run("install", "corndogs", str(chart), "--namespace", CELL,
                         "--set", "storage.backend=file", "--set", "postgresql.enabled=false",
                         "--set", "zalando_postgres.enabled=false", "--set", "tls.enabled=true",
                         "--set", "tls.secretName=corndogs-tls", "--set", "tls.caKey=ca.crt")
        cluster.kube(CELL, "rollout", "status", "deployment/corndogs", "--timeout=300s")

        say("Installing a cell of three heads.")
        cluster.helm_run("install", CELL, str(helm.HEAD_CHART), "--namespace", CELL, *self.image,
                         "--set", "replicas=3", "--set", "storage.tabletVoters=3",
                         "--set", "storage.receiptPolicy=local-quorum",
                         "--set", "replication.listen=127.0.0.1:5200",
                         "--set", f"corndogs.endpoint=corndogs.{CELL}.svc:5080",
                         "--set", "deployment.tls.signingSecret=tallyowl-signing",
                         "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
                         "--set", "dashboard.allowPlaintext=true")
        cluster.kube(CELL, "rollout", "status", f"statefulset/{CELL}", "--timeout=600s")

    def views(self) -> list[GroupView]:
        out = []
        for index in range(3):
            pod = f"{CELL}-{index}"
            samples = self.cluster.metrics(CELL, f"pod/{pod}", 5111)
            out.append(GroupView(
                pod=pod,
                groups=samples.get("tallyowl_consensus_groups_count", 0),
                led=samples.get("tallyowl_consensus_groups_led_count", 0),
                leaderless=samples.get("tallyowl_consensus_groups_leaderless_count", 0),
                lag=samples.get("tallyowl_consensus_replication_lag_count", 0),
            ))
        return out

    def prove_cell(self) -> None:
        verdict: list[str | None] = ["no reading yet"]

        def agreed() -> bool:
            verdict[0] = consensus_verdict(self.views(), groups=2)
            return verdict[0] is None

        try:
            wait_until("The cell forming its groups", agreed, timeout=180, interval=5)
        except ToolFailed as timed_out:
            raise ToolFailed(f"{timed_out} The last reading: {verdict[0]}") from None
        for index in range(3):
            if self.cluster.ready(CELL, f"pod/{CELL}-{index}", 5111).get("ready") is not True:
                raise ToolFailed(f"{CELL}-{index} is not ready with a shared Corndogs.")
        self.passed.append("three heads formed both groups over mutual TLS, ready with a shared Corndogs")

        leader = next(view.pod for view in self.views() if view.led)
        say(f"Deleting the leader {leader}.")
        self.cluster.kube(CELL, "delete", "pod", leader, "--wait=false")

        def took_over() -> bool:
            for index in range(3):
                pod = f"{CELL}-{index}"
                if pod == leader:
                    continue
                samples = self.cluster.metrics(CELL, f"pod/{pod}", 5111)
                if samples.get("tallyowl_consensus_groups_led_count", 0) == 2:
                    return True
            return False

        wait_until("Another head leading both groups", took_over, timeout=120, interval=3)
        self.cluster.kube(CELL, "wait", "--for=condition=Ready", f"pod/{leader}", "--timeout=300s")
        wait_until("The cell agreeing again after the old leader returned", agreed, timeout=180, interval=5)
        self.passed.append(f"the leader {leader} failed over and rejoined as a follower")

    # --- the whole run ------------------------------------------------------

    def run(self) -> int:
        steps: list[tuple[str, Callable[[], None]]] = [
            ("build", self.build),
            ("cluster", self.create_cluster),
            ("certificates", self.certificates),
            ("home install", self.install_home),
            ("data path", self.prove_data_path),
            ("refusal", self.prove_refusal),
            ("rotation", self.prove_rotation),
            ("cell install", self.install_cell),
            ("cell", self.prove_cell),
        ]
        cluster_made = False
        try:
            for name, step in steps:
                say(f"Step: {name}")
                try:
                    step()
                except ToolFailed:
                    if cluster_made:
                        self.cluster.diagnose()
                    raise
                cluster_made = cluster_made or name == "cluster"
        finally:
            if cluster_made and not self.keep:
                say(f"Deleting the cluster `{CLUSTER}`.")
                self.cluster.kind_run("delete", "cluster", "--name", CLUSTER, check=False)
            elif cluster_made:
                say(f"The cluster `{CLUSTER}` is kept. Its kubeconfig is gone with the temporary "
                    f"directory; `kind export kubeconfig --name {CLUSTER}` writes one, and "
                    f"`kind delete cluster --name {CLUSTER}` removes it.")
        say("kind-check passed:")
        for proved in self.passed:
            print(f"  - {proved}")
        return 0


def kind_check(keep: bool = False) -> int:
    """Install both charts in a disposable cluster and prove them. See the module text."""
    with tempfile.TemporaryDirectory(prefix="tallyowl-kind-check-") as workdir:
        return Check(Path(workdir), keep).run()
