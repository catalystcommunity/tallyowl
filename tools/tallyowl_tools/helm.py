"""Chart rendering checks, with no cluster.

`docs/DEPLOYMENT.md` section 8 requires two kinds of check this module gives:

- every profile renders, and the rendered configuration is the values tree, so
  the chart-parity test in `crates/tallyowl-config` keeps protecting it;
- a configuration TallyOwl would refuse at start is refused at render, which
  reaches the operator at `helm install` rather than after a deploy.

The install, upgrade, and rollback test in a disposable cluster is
`docs/DEPLOYMENT.md` section 8 item 7 and runs from the Reactorcide integration
job, not from here: this module needs `helm` and nothing else.
"""

from __future__ import annotations

from . import deps
from pathlib import Path

from .commands import REPOSITORY_ROOT, ToolFailed, run, say

HEAD_CHART = REPOSITORY_ROOT / "charts" / "tallyowl"
COLLECTOR_CHART = REPOSITORY_ROOT / "charts" / "tallyowl-collector"

#: Overrides that describe one replicated cell: three voters, a quorum receipt
#: policy, and an address peers can reach. docs/DEPLOYMENT.md section 3.
REPLICATED = [
    "--set", "installation.profile=replicated",
    "--set", "cell.controllers=3",
    "--set", "replicas=3",
    "--set", "storage.tabletVoters=3",
    "--set", "storage.receiptPolicy=local-quorum",
    "--set", "replication.listen=0.0.0.0:5200",
]

#: Where a collector pod finds the queue and the head. The collector chart keeps
#: the loader's loopback defaults in its settings tree, so that the chart-parity
#: test still reads a valid home configuration, and refuses them at render: in a
#: collector pod a loopback address reaches nothing and the collector stops at
#: start. Every collector render that is meant to succeed names both.
COLLECTOR_REACHABLE = [
    "--set", "corndogs.endpoint=home-corndogs:5080",
    "--set", "head.endpoint=home:5110",
]

#: The transport security of D62. In a pod every CSIL listener and every hop to
#: another service crosses the pod network, so a head needs an authority to sign
#: with and the authorities to trust, and a collector needs a certificate for
#: applications, the authorities, and a role token. Each chart refuses a render
#: with neither these nor `transport.allowPlaintext`. Every render that is meant
#: to succeed names them, and so does every refusal case that is meant to fail
#: for a different reason. The dashboard is plaintext behind a gateway (the
#: D62 exception), and a render with no Gateway says so with its own value.
HEAD_SECURE = [
    "--set", "deployment.tls.signingSecret=tallyowl-signing",
    "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
    "--set", "dashboard.allowPlaintext=true",
    # The certificate a Corndogs beside the head serves. It changes nothing
    # when `corndogsDeployment.enabled` is off.
    "--set", "corndogsDeployment.tlsSecret=corndogs-tls",
]
COLLECTOR_SECURE = [
    "--set", "deployment.tls.certificateSecrets[0]=collector-tls",
    "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
    "--set", "deployment.tls.roleTokenSecret.name=collector-role-token",
]

#: Each refusal the charts must make at render, from docs/DEPLOYMENT.md
#: sections 4 and 8. The fragment is what the failure message must name, so a
#: refusal that fires for the wrong reason still fails this check.
HEAD_REFUSALS: list[tuple[str, list[str], str]] = [
    (
        "local-one with three voters",
        [*HEAD_SECURE, "--set", "storage.tabletVoters=3", "--set", "replication.listen=0.0.0.0:5200"],
        "local-one",
    ),
    (
        "three voters with no replication address",
        [*HEAD_SECURE, "--set", "storage.tabletVoters=3", "--set", "storage.receiptPolicy=local-quorum"],
        "replication.listen",
    ),
    (
        "two durable copies on the file backend",
        [*HEAD_SECURE, "--set", "corndogs.durableCopies=2"],
        "durableCopies",
    ),
    (
        "a gcGrace shorter than the longest query",
        [*HEAD_SECURE, "--set", "compaction.gcGrace=10s"],
        "gcGrace",
    ),
    (
        "a merge threshold that is not below half the split threshold",
        [*HEAD_SECURE, "--set", "placement.mergeBelow=32GiB"],
        "mergeBelow",
    ),
    (
        "a deduplication window the retry window outlives",
        [*HEAD_SECURE, "--set", "storage.deduplicationWindow=12h"],
        "deduplicationWindow",
    ),
    (
        "three replicas with one tablet voter",
        [*HEAD_SECURE, "--set", "replicas=3"],
        "tabletVoters",
    ),
    (
        "a disruption budget below a majority of five voters",
        [*HEAD_SECURE, 
            "--set", "replicas=5",
            "--set", "storage.tabletVoters=5",
            "--set", "storage.receiptPolicy=local-quorum",
            "--set", "replication.listen=0.0.0.0:5200",
            "--set", "disruption.minAvailable=2",
        ],
        "disruption.minAvailable",
    ),
    (
        "a bind address as a peer address",
        [*HEAD_SECURE, *REPLICATED, "--set", "replication.peers=0.0.0.0:5200"],
        "replication.peers",
    ),
    (
        "the queue beside the head with no volume under it",
        [*HEAD_SECURE, "--set", "corndogsDeployment.enabled=true", "--set", "persistence.enabled=false"],
        "persistence.enabled",
    ),
    (
        "a queue beside the head that collectors would reach in plaintext",
        [*HEAD_SECURE,
            "--set", "corndogsDeployment.enabled=true",
            "--set", "corndogsDeployment.tlsSecret=",
        ],
        "corndogsDeployment.tlsSecret",
    ),
    (
        "a queue image whose version can change under a running installation",
        [*HEAD_SECURE, 
            "--set", "corndogsDeployment.enabled=true",
            "--set", "corndogsDeployment.image=example.test/corndogs:latest",
        ],
        "corndogsDeployment.image",
    ),
    (
        "self-observation with no collector address to send to",
        [*HEAD_SECURE, "--set", "metrics.selfObservation.enabled=true"],
        "deployment.selfObservationCollector",
    ),
    (
        "a maintenance Job with no verb",
        [*HEAD_SECURE, "--set", "deployment.maintenance.enabled=true"],
        "deployment.maintenance.args",
    ),
    (
        "a head on the pod network with no authority to sign with or trust",
        ["--set", "dashboard.allowPlaintext=true"],
        "deployment.tls.signingSecret",
    ),
    (
        "a plaintext dashboard exposed with no Gateway in front of it",
        [
            "--set", "deployment.tls.signingSecret=tallyowl-signing",
            "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
        ],
        "dashboard.allowPlaintext",
    ),
    (
        "a replicated node name longer than one DNS label",
        [
            *HEAD_SECURE, *REPLICATED,
            "--namespace", "a-namespace-whose-name-is-long-enough-to-overflow",
        ],
        "at most 63",
    ),
]

COLLECTOR_REFUSALS: list[tuple[str, list[str], str]] = [
    (
        "a batch seal the queue payload cannot carry",
        [*COLLECTOR_SECURE, *COLLECTOR_REACHABLE, "--set", "corndogs.maxPayloadBytes=256KiB"],
        "maxPayloadBytes",
    ),
    (
        "two durable copies on the file backend",
        [*COLLECTOR_SECURE, *COLLECTOR_REACHABLE, "--set", "corndogs.durableCopies=2"],
        "durableCopies",
    ),
    (
        "a queue address that is loopback inside a collector pod",
        [*COLLECTOR_SECURE, "--set", "head.endpoint=home:5110"],
        "corndogs.endpoint",
    ),
    (
        "a head address that is loopback inside a collector pod",
        [*COLLECTOR_SECURE, "--set", "corndogs.endpoint=home-corndogs:5080"],
        "head.endpoint",
    ),
    (
        "autoscaling on CPU with no CPU request to measure against",
        [*COLLECTOR_SECURE, 
            *COLLECTOR_REACHABLE,
            "--set", "autoscaling.enabled=true",
            "--set", "resources.requests.cpu=null",
        ],
        "resources.requests.cpu",
    ),
    (
        "a compatibility receiver with no key to present",
        [*COLLECTOR_SECURE, *COLLECTOR_REACHABLE, "--set", "compatibility.openTelemetry.enabled=true"],
        "deployment.apiKeySecret.name",
    ),
    (
        "an intake on the pod network with no certificate for applications",
        [
            *COLLECTOR_REACHABLE,
            "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
            "--set", "deployment.tls.roleTokenSecret.name=collector-role-token",
        ],
        "deployment.tls.certificateSecrets",
    ),
    (
        "a collector that cannot enroll with the head it reaches",
        [*COLLECTOR_REACHABLE, "--set", "deployment.tls.certificateSecrets[0]=collector-tls"],
        "deployment.tls.roleTokenSecret.name",
    ),
]


#: Top-level values that are deployment concerns rather than TallyOwl settings.
#: The chart's `settings` helper omits each one, and the loader would call any
#: of them an unknown key. Keep this equal to NOT_SETTINGS in
#: `crates/tallyowl-config/tests/chart_parity.rs` and to the `omit` list in each
#: chart's `_helpers.tpl`.
DEPLOYMENT_KEYS = (
    "image",
    "replicas",
    "resources",
    "persistence",
    "topologySpread",
    "affinity",
    "disruption",
    "autoscaling",
    "corndogsDeployment",
    "securityContext",
    "bindAddress",
    "dashboardAssets",
    "gateway",
    "deployment",
)


#: What a rendered pod must bind, and where it must find the dashboard. A
#: loopback bind in a pod reaches nothing and the Service in front of it
#: forwards to a port no client can use, which is a deployment that looks
#: healthy and serves nobody.
EXPECTED_BINDS = {
    "tallyowl": {"head": ("listen", "operationalListen"), "dashboard": ("listen",)},
    "tallyowl-collector": {"collector": ("listen", "operationalListen")},
}


def check_rendered_binds(rendered: str, chart: str) -> None:
    """Every listening address in the rendered settings faces the network."""
    import yaml

    settings = _rendered_settings(rendered, chart)
    for section, keys in EXPECTED_BINDS[chart].items():
        for key in keys:
            value = (settings.get(section) or {}).get(key)
            if not value:
                continue
            host = str(value).rsplit(":", 1)[0]
            if host in ("127.0.0.1", "localhost", "::1"):
                raise ToolFailed(
                    f"The {chart} chart renders `{section}.{key}` as {value}. A pod "
                    "that binds loopback reaches nothing, and the Service in front "
                    "of it forwards to a port no client can use. The `settings` "
                    "helper rewrites the host from `bindAddress`."
                )
    assets = (settings.get("dashboard") or {}).get("assets", "")
    if settings.get("dashboard", {}).get("enabled") and not str(assets).startswith("/"):
        raise ToolFailed(
            f"The {chart} chart renders `dashboard.assets` as {assets!r}, which is "
            "relative. A pod resolves it against its working directory rather than "
            "against the image, so the dashboard serves nothing. The bundle is at "
            "the path `dashboardAssets` names."
        )
    say(f"The {chart} settings bind to the network, and the dashboard path is absolute.")


def _rendered_settings(rendered: str, chart: str) -> dict:
    """The settings document out of a rendered chart's ConfigMap."""
    import yaml

    for document in yaml.safe_load_all(rendered):
        if not document or document.get("kind") != "ConfigMap":
            continue
        for name, body in (document.get("data") or {}).items():
            if name.endswith(".yaml"):
                return yaml.safe_load(body) or {}
    raise ToolFailed(
        f"The {chart} chart rendered no settings ConfigMap, and the configuration "
        "a pod reads is that file."
    )


def check_rendered_settings(rendered: str, chart: str) -> None:
    """Read what the ConfigMap actually holds, not what the values hold.

    The chart-parity test in `crates/tallyowl-config` filters the deployment
    keys out of the values **before** it loads them, so it cannot see one that
    the template forgot to omit. Nothing else read the rendered ConfigMap, and
    a deployment key that reaches it makes `tallyowl-head config check` fail on
    a stock installation — the one command an operator has for checking a
    configuration. This closes that gap where it opened: in the rendered
    output.
    """
    settings = _rendered_settings(rendered, chart)

    leaked = [key for key in DEPLOYMENT_KEYS if key in settings]
    if leaked:
        raise ToolFailed(
            f"The {chart} chart wrote {', '.join(leaked)} into the settings "
            "ConfigMap. The loader knows no such setting, so `config check` "
            "fails on every installation. Add each one to the `omit` list in "
            f"charts/{chart}/templates/_helpers.tpl."
        )
    say(f"The {chart} settings hold {len(settings)} keys, and no deployment key.")


#: The file each mounted path stands for, when a render is checked on this host.
#: `tallyowl.securityMounts` and the collector Deployment mount the Secrets of
#: `deployment.tls` here.
MOUNTED_FILES = (
    (r"/etc/tallyowl-signing/tls\.crt", "intermediate.crt"),
    (r"/etc/tallyowl-signing/tls\.key", "intermediate.key"),
    (r"/etc/tallyowl-authorities/\d+/[A-Za-z0-9._-]+", "root.crt"),
    (r"/etc/tallyowl-certificates/\d+", "collector"),
)


def pod_configuration(rendered: str, chart: str, files: Path, release: str) -> tuple[str, dict[str, str]]:
    """The configuration file and the environment one pod of a render starts with.

    The settings come out of the ConfigMap, with each mounted path pointed at a
    file under `files`. The environment is what the kubelet gives the first
    container: `$(POD_NAME)` becomes `<release>-0`, a field reference becomes a
    plain value, and a Secret reference becomes a placeholder, because
    `config check` parses a reference and never reads the value.
    """
    import re

    import yaml

    settings = yaml.safe_dump(_rendered_settings(rendered, chart), sort_keys=False)
    for pattern, name in MOUNTED_FILES:
        settings = re.sub(pattern, str(files / name), settings)

    pod = f"{release}-0"
    container = _workload(rendered, chart)["spec"]["template"]["spec"]["containers"][0]
    environment: dict[str, str] = {}
    for name, entry in _environment(container).items():
        if "value" in entry:
            environment[name] = str(entry["value"]).replace("$(POD_NAME)", pod)
            continue
        source = entry.get("valueFrom") or {}
        field = (source.get("fieldRef") or {}).get("fieldPath")
        if field == "metadata.name":
            environment[name] = pod
        elif field:
            environment[name] = "host-a"
        else:
            environment[name] = "a-placeholder-for-a-secret"
    return settings, environment


def check_corndogs_tls(head: str, collector: str) -> None:
    """The home profile's hop to Corndogs is TLS at both ends (D62).

    The sidecar serves the certificate from its Secret, the head reaches it by
    the Service name on that certificate, and the collector trusts the
    installation authority for it.
    """
    for needed in ("CORNDOGS_TLS_CERT_FILE", "CORNDOGS_TLS_KEY_FILE", "secretName: corndogs-tls"):
        if needed not in head:
            raise ToolFailed(f"The head chart put the Corndogs sidecar on the pod network without `{needed}`.")
    if "serverName: home-corndogs." not in head:
        raise ToolFailed("The head chart does not reach its Corndogs sidecar by the Service name on its certificate.")
    if "caFile: /etc/tallyowl-authorities/0/" not in collector:
        raise ToolFailed("The collector chart does not trust the installation authority for Corndogs.")
    if "allowPlaintext" in _yaml_block(collector, "corndogs"):
        raise ToolFailed("The collector chart still sets a plaintext exception for Corndogs.")


def check_sidecar_pull_policy(rendered: str, head: str, corndogs: str) -> None:
    """The head and its Corndogs sidecar each pull by the policy given.

    One policy used to cover both images. A TallyOwl image built locally needs
    `Never`, and the Corndogs image from its registry cannot use it, so a pod
    that pulled nothing waited on an image it would never have.
    """
    containers = {
        container["name"]: container.get("imagePullPolicy")
        for container in _workload(rendered, "tallyowl")["spec"]["template"]["spec"]["containers"]
    }
    for name, wanted in (("head", head), ("corndogs", corndogs)):
        if containers.get(name) != wanted:
            raise ToolFailed(
                f"The `{name}` container pulls with `{containers.get(name)}`, and the values ask for `{wanted}`."
            )


def _yaml_block(rendered: str, key: str) -> str:
    """The lines under the first `key:` of a rendered document, by indentation."""
    lines = rendered.splitlines()
    for index, line in enumerate(lines):
        if line.strip() == f"{key}:":
            indent = len(line) - len(line.lstrip())
            block = []
            for following in lines[index + 1:]:
                if following.strip() and len(following) - len(following.lstrip()) <= indent:
                    break
                block.append(following)
            return "\n".join(block)
    return ""


def check_config_accepted(
    rendered: str, chart: str, binary: Path, files: Path, release: str, profile: str
) -> None:
    """The service accepts the configuration a render gives it.

    The chart refusals copy the loader's rules, and a copy drifts: a chart once
    bound every listener of both services to the pod address, and after D62 no
    rendered configuration passed the service's own `config check`, while every
    render and every refusal here still passed. This runs the real binary on
    the real rendered configuration, so the chart and the loader cannot
    disagree without this failing.

    The binary runs with a clean environment rather than through `run`, which
    adds the environment of this host: a `TALLYOWL_` variable on a workstation
    would otherwise decide what the check sees.
    """
    import os
    import subprocess
    import tempfile

    settings, environment = pod_configuration(rendered, chart, files, release)
    with tempfile.TemporaryDirectory(prefix="tallyowl-helm-check-") as scratch:
        config = os.path.join(scratch, "tallyowl.yaml")
        with open(config, "w", encoding="utf-8") as handle:
            handle.write(settings)
        clean = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), **environment}
        completed = subprocess.run(  # noqa: S603 - an array, never a shell string
            # `config check` first: the collector reads a verb only there.
            [str(binary), "config", "check", "--config", config],
            env=clean,
            cwd=scratch,
            capture_output=True,
            text=True,
            check=False,
        )
    if completed.returncode != 0:
        raise ToolFailed(
            f"The {chart} chart renders the {profile} profile, and `{binary.name} config "
            "check` refuses the configuration it renders, so every pod of it stops at "
            "start. Change the chart so it renders what the service accepts. The check "
            "said:\n" + (completed.stdout + completed.stderr).strip()
        )
    say(f"`{binary.name} config check` accepts the {chart} {profile} render.")


def _service_binaries() -> tuple[Path, Path]:
    """The two service binaries, built when this checkout has none."""
    from .build import _cargo

    target = REPOSITORY_ROOT / "target" / "debug"
    head = target / "tallyowl-head"
    collector = target / "tallyowl-collector"
    if not (head.is_file() and collector.is_file()):
        run([_cargo(), "build", "-p", "tallyowl-head", "-p", "tallyowl-collector"])
    return head, collector


def _test_authority(head: Path, into: Path) -> Path:
    """A root and an intermediate from the head's own `ca create`, and a
    certificate directory for a collector made from the same files.

    `config check` parses the settings and does not open these files. They are
    real so that the paths a render names are paths that exist.
    """
    import shutil

    authority = into / "authority"
    run([head, "ca", "create", authority], capture=True)
    collector = authority / "collector"
    collector.mkdir()
    shutil.copyfile(authority / "intermediate.crt", collector / "tls.crt")
    shutil.copyfile(authority / "intermediate.key", collector / "tls.key")
    return authority


#: Hosts that a process binds and that nothing can dial.
NOT_DIALABLE = ("0.0.0.0", "127.0.0.1", "localhost", "::", "[::]", "::1", "[::1]")


def node_name_for(address: str) -> str:
    """What a node calls a peer, from the peer's address.

    This is the rule in `crates/tallyowl-head/src/cluster.rs`, and a consensus
    ID is a hash of the name it gives. A chart that names a pod any other way
    gives each node a different voter set, and no group forms.
    """
    return "node-" + address.replace(".", "-").replace(":", "-")


def _documents(rendered: str) -> list[dict]:
    import yaml

    return [document for document in yaml.safe_load_all(rendered) if document]


def _workload(rendered: str, chart: str) -> dict:
    for document in _documents(rendered):
        if document.get("kind") in ("StatefulSet", "Deployment"):
            return document
    raise ToolFailed(f"The {chart} chart rendered no StatefulSet and no Deployment.")


def _environment(container: dict) -> dict[str, dict]:
    return {entry["name"]: entry for entry in container.get("env") or []}


def check_replicated_identity(rendered: str, release: str, namespace: str, replicas: int) -> None:
    """Every node and its peers agree on each node's name and address.

    A replicated install from this chart once could not form a group: a pod
    named itself `<release>-0`, its peers named it from its address, the peer
    list was shared and held the pod itself, and the pod advertised the address
    it bound, 0.0.0.0. Every pod became ready and no write committed.
    """
    settings = _rendered_settings(rendered, "tallyowl")
    peers = [
        peer.strip()
        for peer in str((settings.get("replication") or {}).get("peers") or "").split(",")
        if peer.strip()
    ]
    port = str(settings["replication"]["listen"]).rsplit(":", 1)[-1]
    expected = [
        f"{release}-{ordinal}.{release}-nodes.{namespace}.svc:{port}" for ordinal in range(replicas)
    ]
    if peers != expected:
        raise ToolFailed(
            f"The chart renders `replication.peers` as {peers}, and {replicas} replicas "
            f"need {expected}: one address for each pod, through the headless Service."
        )
    for peer in peers:
        if peer.rsplit(":", 1)[0] in NOT_DIALABLE:
            raise ToolFailed(
                f"The chart renders the peer address {peer}. A process binds that "
                "address and no other node can dial it."
            )

    container = _workload(rendered, "tallyowl")["spec"]["template"]["spec"]["containers"][0]
    names = [entry["name"] for entry in container.get("env") or []]
    environment = _environment(container)
    advertise = (environment.get("TALLYOWL_REPLICATION__ADVERTISE") or {}).get("value", "")
    name = (environment.get("TALLYOWL_NODE__NAME") or {}).get("value", "")
    if "$(POD_NAME)" not in advertise or advertise.split(".", 1)[0] != "$(POD_NAME)":
        raise ToolFailed(
            "The chart does not give each pod its own `replication.advertise`. It "
            f"rendered {advertise!r}, and the address must start with $(POD_NAME)."
        )
    if "POD_NAME" not in names or names.index("POD_NAME") > names.index(
        "TALLYOWL_REPLICATION__ADVERTISE"
    ):
        raise ToolFailed(
            "POD_NAME must come before the variables that use it. The kubelet "
            "expands $(POD_NAME) only from a variable that is already defined."
        )
    for ordinal, peer in enumerate(expected):
        pod = f"{release}-{ordinal}"
        if advertise.replace("$(POD_NAME)", pod) != peer:
            raise ToolFailed(
                f"Pod {pod} advertises {advertise.replace('$(POD_NAME)', pod)} and "
                f"its peers dial {peer}. The two must be one address."
            )
        if name.replace("$(POD_NAME)", pod) != node_name_for(peer):
            raise ToolFailed(
                f"Pod {pod} names itself {name.replace('$(POD_NAME)', pod)} and its "
                f"peers name it {node_name_for(peer)}. A consensus ID is a hash of "
                "the name, so the two never see one voter set."
            )

    for document in _documents(rendered):
        if document.get("kind") == "Service" and document["metadata"]["name"] == f"{release}-nodes":
            if not document["spec"].get("publishNotReadyAddresses"):
                raise ToolFailed(
                    "The headless Service does not publish addresses that are not "
                    "ready. A group forms before any pod is ready, so a peer could "
                    "not resolve another."
                )
            break
    else:
        raise ToolFailed(f"The chart rendered no headless Service named {release}-nodes.")
    say(f"All {replicas} nodes and their peers agree on every name and address.")


def check_pod_hardening(rendered: str, chart: str) -> None:
    """A namespace that enforces the `restricted` profile accepts the pod."""
    spec = _workload(rendered, chart)["spec"]["template"]["spec"]
    if (spec.get("securityContext") or {}).get("seccompProfile", {}).get("type") != "RuntimeDefault":
        raise ToolFailed(f"The {chart} pod has no RuntimeDefault seccomp profile.")
    if not spec.get("terminationGracePeriodSeconds"):
        raise ToolFailed(
            f"The {chart} pod sets no terminationGracePeriodSeconds, so a drain "
            "has the default 30 seconds however long it needs."
        )
    for container in spec["containers"]:
        name = container["name"]
        context = container.get("securityContext") or {}
        if context.get("allowPrivilegeEscalation") is not False:
            raise ToolFailed(f"The {chart} container `{name}` may escalate privilege.")
        if "ALL" not in ((context.get("capabilities") or {}).get("drop") or []):
            raise ToolFailed(f"The {chart} container `{name}` does not drop every capability.")
        if not ((container.get("resources") or {}).get("requests") or {}).get("memory"):
            raise ToolFailed(
                f"The {chart} container `{name}` has no memory request, so the pod "
                "is BestEffort and the node evicts it first."
            )
        image = str(container["image"])
        tag = image.rsplit(":", 1)[-1] if ":" in image.rsplit("/", 1)[-1] else ""
        if tag in ("", "latest"):
            raise ToolFailed(
                f"The {chart} container `{name}` runs {image}, and that version "
                "changes when a pod moves to another node."
            )
    if "startupProbe" not in spec["containers"][0]:
        raise ToolFailed(f"The {chart} container has no startupProbe.")
    say(f"The {chart} pod meets the `restricted` profile and every image is pinned.")


def check_key_secret(rendered: str, chart: str, secret: str) -> None:
    """The key reaches the process as a reference, and never as a value."""
    settings = _rendered_settings(rendered, chart)
    reference = (settings.get("collector") or {}).get("apiKey")
    if reference != "env:SECRET_TALLYOWL_API_KEY":
        raise ToolFailed(
            f"The {chart} chart renders `collector.apiKey` as {reference!r} with "
            "a key Secret named. It must be `env:SECRET_TALLYOWL_API_KEY`."
        )
    container = _workload(rendered, chart)["spec"]["template"]["spec"]["containers"][0]
    entry = _environment(container).get("SECRET_TALLYOWL_API_KEY") or {}
    if ((entry.get("valueFrom") or {}).get("secretKeyRef") or {}).get("name") != secret:
        raise ToolFailed(
            f"The {chart} pod does not fill SECRET_TALLYOWL_API_KEY from the Secret "
            f"`{secret}`, so the reference in the settings points at nothing."
        )
    if "value" in entry:
        raise ToolFailed(f"The {chart} pod carries the key as a literal value.")
    say(f"The {chart} key is a reference to the Secret `{secret}`.")


def check_disruption_budget(rendered: str, replicas: int) -> None:
    """A voluntary disruption never takes a majority of the voters."""
    majority = replicas // 2 + 1
    for document in _documents(rendered):
        if document.get("kind") == "PodDisruptionBudget":
            held = document["spec"].get("minAvailable")
            if held != majority:
                raise ToolFailed(
                    f"The chart renders minAvailable {held} for {replicas} replicas, "
                    f"and a majority is {majority}."
                )
            say(f"The disruption budget keeps {majority} of {replicas} replicas.")
            return
    raise ToolFailed(f"The chart rendered no disruption budget for {replicas} replicas.")


def check_self_observation_target(rendered: str) -> None:
    """The head dials `metrics.selfObservation.endpoint`, so it is never a bind address.

    An empty endpoint falls back to `collector.listen`, which the chart rewrites
    to the pod bind address. That is the defect this check exists to catch.
    """
    settings = _rendered_settings(rendered, "tallyowl")
    observation = (settings.get("metrics") or {}).get("selfObservation") or {}
    listen = str(observation.get("endpoint") or (settings.get("collector") or {}).get("listen"))
    if listen.rsplit(":", 1)[0] in NOT_DIALABLE:
        raise ToolFailed(
            f"Self-observation is on and the head would dial {listen}. That is an "
            "address a process binds, and nothing answers there."
        )
    say(f"The head sends its own metrics to {listen}.")


def check_maintenance(rendered: str, release: str) -> None:
    """The Job has the volume, and the head does not hold the lock on it."""
    workload = _workload(rendered, "tallyowl")
    if workload["spec"]["replicas"] != 0:
        raise ToolFailed(
            "A maintenance Job is enabled and the head still has replicas. The head "
            "holds the lock on the data directory, so the Job can never open it."
        )
    for document in _documents(rendered):
        if document.get("kind") != "Job":
            continue
        claims = [
            (volume.get("persistentVolumeClaim") or {}).get("claimName")
            for volume in document["spec"]["template"]["spec"]["volumes"]
        ]
        if f"data-{release}-0" not in claims:
            raise ToolFailed(f"The maintenance Job does not mount data-{release}-0: {claims}.")
        say("The maintenance Job mounts the data volume, and the head is stopped.")
        return
    raise ToolFailed("`deployment.maintenance.enabled` rendered no Job.")


GATEWAY_REFUSALS = (
    (
        "a route with no Gateway to attach to",
        [*HEAD_SECURE, "--set", "gateway.enabled=true"],
        "gateway.parentRef.name",
    ),
    (
        "a dashboard route with the dashboard turned off",
        [
            *HEAD_SECURE,
            "--set", "gateway.enabled=true",
            "--set", "gateway.parentRef.name=platform",
            "--set", "dashboard.enabled=false",
        ],
        "There is nothing behind that route",
    ),
)


def check_gateway_refusals(helm: str) -> None:
    """A route that leads nowhere is refused at render, not at request time."""
    for name, overrides, fragment in GATEWAY_REFUSALS:
        result = run(
            [helm, "template", "refused", str(HEAD_CHART), *overrides],
            capture=True,
            check=False,
        )
        if result.ok:
            raise ToolFailed(f"The chart rendered {name}, and it must refuse it.")
        if fragment not in result.stderr:
            raise ToolFailed(f"The chart refused {name} without naming `{fragment}`.")

    # A collector serves no dashboard, and saying so beats rendering a route to
    # a port that answers nothing.
    result = run(
        [
            helm, "template", "refused", str(COLLECTOR_CHART),
            *COLLECTOR_REACHABLE, *COLLECTOR_SECURE,
            "--set", "gateway.enabled=true",
            "--set", "gateway.parentRef.name=platform",
            "--set", "gateway.dashboard.enabled=true",
        ],
        capture=True,
        check=False,
    )
    if result.ok or "collector serves no dashboard" not in result.stderr:
        raise ToolFailed(
            "The collector chart accepted a dashboard route. It has no dashboard."
        )
    say("Every gateway refusal refuses, and names what is wrong.")


def check() -> int:
    """Lint both charts, render every profile, and prove every refusal."""
    import tempfile

    helm = deps.helm_program()
    head_binary, collector_binary = _service_binaries()
    binaries = {"tallyowl": head_binary, "tallyowl-collector": collector_binary}
    #: Every render that must succeed, as (chart, release, profile, output), so
    #: that the real `config check` reads each one at the end.
    accepted: list[tuple[str, str, str, str]] = []

    run([helm, "lint", str(HEAD_CHART), *HEAD_SECURE], quiet=True)
    run([helm, "lint", str(COLLECTOR_CHART), *COLLECTOR_REACHABLE, *COLLECTOR_SECURE], quiet=True)

    say("Rendering the home profile: a head with the queue beside it, and a collector that names its queue and its head.")
    head = run(
        [
            helm, "template", "home", str(HEAD_CHART), *HEAD_SECURE,
            "--set", "corndogsDeployment.enabled=true",
        ],
        capture=True,
    )
    collector = run(
        [helm, "template", "home", str(COLLECTOR_CHART), *COLLECTOR_REACHABLE, *COLLECTOR_SECURE],
        capture=True,
    )
    accepted += [
        ("tallyowl", "home", "home", head.stdout),
        ("tallyowl-collector", "home", "home", collector.stdout),
    ]

    check_rendered_settings(head.stdout, "tallyowl")
    check_rendered_settings(collector.stdout, "tallyowl-collector")
    check_corndogs_tls(head.stdout, collector.stdout)
    check_sidecar_pull_policy(head.stdout, "IfNotPresent", "IfNotPresent")
    say("Rendering the Corndogs sidecar with its own pull policy, and with the head's.")
    for image_policy, sidecar_policy, expected in (
        ("Never", "", "Never"),
        ("Never", "IfNotPresent", "IfNotPresent"),
    ):
        policy_render = run(
            [
                helm, "template", "home", str(HEAD_CHART), *HEAD_SECURE,
                "--set", "corndogsDeployment.enabled=true",
                "--set", f"image.pullPolicy={image_policy}",
                "--set", f"corndogsDeployment.imagePullPolicy={sidecar_policy}",
            ],
            capture=True,
        )
        check_sidecar_pull_policy(policy_render.stdout, image_policy, expected)
    check_rendered_binds(head.stdout, "tallyowl")
    check_rendered_binds(collector.stdout, "tallyowl-collector")
    check_pod_hardening(head.stdout, "tallyowl")
    check_pod_hardening(collector.stdout, "tallyowl-collector")

    say("Rendering both charts in plaintext, which an operator has to ask for.")
    plain_head = run(
        [
            helm, "template", "home", str(HEAD_CHART),
            "--set", "transport.allowPlaintext=true",
            "--set", "dashboard.allowPlaintext=true",
        ],
        capture=True,
    )
    plain_collector = run(
        [
            helm, "template", "home", str(COLLECTOR_CHART), *COLLECTOR_REACHABLE,
            "--set", "transport.allowPlaintext=true",
        ],
        capture=True,
    )
    accepted += [
        ("tallyowl", "home", "plaintext", plain_head.stdout),
        ("tallyowl-collector", "home", "plaintext", plain_collector.stdout),
    ]

    say("Rendering both charts with a key Secret, and the head with self-observation.")
    keyed = ["--set", "deployment.apiKeySecret.name=tallyowl-key"]
    observed = run(
        [
            helm, "template", "home", str(HEAD_CHART), *HEAD_SECURE, *keyed,
            "--set", "metrics.selfObservation.enabled=true",
            "--set", "deployment.selfObservationCollector=home-collector:5100",
        ],
        capture=True,
    )
    check_key_secret(observed.stdout, "tallyowl", "tallyowl-key")
    check_self_observation_target(observed.stdout)
    receiving = run(
        [
            helm, "template", "home", str(COLLECTOR_CHART), *COLLECTOR_REACHABLE,
            *COLLECTOR_SECURE, *keyed,
            "--set", "compatibility.openTelemetry.enabled=true",
            # A second certificate, as during a rotation.
            "--set", "deployment.tls.certificateSecrets[1]=collector-tls-next",
        ],
        capture=True,
    )
    check_key_secret(receiving.stdout, "tallyowl-collector", "tallyowl-key")
    accepted += [
        ("tallyowl", "home", "self-observation", observed.stdout),
        ("tallyowl-collector", "home", "OpenTelemetry receiver", receiving.stdout),
    ]

    say("Rendering a maintenance Job.")
    maintained = run(
        [
            helm, "template", "home", str(HEAD_CHART), *HEAD_SECURE,
            "--set", "deployment.maintenance.enabled=true",
            "--set", "deployment.maintenance.args={provision,shop}",
        ],
        capture=True,
    )
    check_maintenance(maintained.stdout, "home")

    say("Rendering every optional object.")
    for chart, overrides in (
        (HEAD_CHART, [*HEAD_SECURE]),
        (COLLECTOR_CHART, [*COLLECTOR_REACHABLE, *COLLECTOR_SECURE, "--set", "replicas=2"]),
    ):
        optional = run(
            [
                helm, "template", "home", str(chart), *overrides,
                "--set", "deployment.networkPolicy.enabled=true",
                "--set", "deployment.serviceMonitor.enabled=true",
            ],
            capture=True,
        )
        for kind in ("NetworkPolicy", "ServiceMonitor"):
            if f"kind: {kind}" not in optional.stdout:
                raise ToolFailed(f"{chart.name} rendered no {kind} when it was turned on.")
    if "kind: PodDisruptionBudget" not in optional.stdout:
        raise ToolFailed("Two collectors rendered no disruption budget.")

    say("Rendering the Gateway API routes.")
    for chart, overrides in (
        (
            HEAD_CHART,
            [
                "--set", "deployment.tls.signingSecret=tallyowl-signing",
                "--set", "deployment.tls.authorities[0].secretName=tallyowl-authority",
                "--set", "gateway.enabled=true",
                "--set", "gateway.parentRef.name=platform",
            ],
        ),
        (
            COLLECTOR_CHART,
            [
                *COLLECTOR_REACHABLE, *COLLECTOR_SECURE,
                "--set", "gateway.enabled=true",
                "--set", "gateway.parentRef.name=platform",
                "--set", "gateway.operational.enabled=true",
            ],
        ),
    ):
        routed = run([helm, "template", "routed", str(chart), *overrides], capture=True)
        if "kind: HTTPRoute" not in routed.stdout:
            raise ToolFailed(
                f"{chart.name} rendered no HTTPRoute with `gateway.enabled=true`."
            )
        if "kind: Ingress" in routed.stdout:
            raise ToolFailed(
                f"{chart.name} rendered an Ingress. This chart routes with Gateway "
                "API; an Ingress template is a separate decision nobody has made."
            )
        accepted.append((chart.name, "routed", "Gateway", routed.stdout))

    say("Rendering one replicated cell.")
    cell = run(
        [
            helm, "template", "cell", str(HEAD_CHART), "--namespace", "telemetry",
            *HEAD_SECURE, *REPLICATED,
        ],
        capture=True,
    )
    check_replicated_identity(cell.stdout, "cell", "telemetry", 3)
    check_disruption_budget(cell.stdout, 3)
    accepted.append(("tallyowl", "cell", "replicated", cell.stdout))
    five = run(
        [
            helm, "template", "cell", str(HEAD_CHART), *HEAD_SECURE, *REPLICATED,
            "--set", "replicas=5", "--set", "storage.tabletVoters=5",
        ],
        capture=True,
    )
    check_disruption_budget(five.stdout, 5)

    say("Rendering the collector with autoscaling on.")
    run(
        [
            helm, "template", "scaled", str(COLLECTOR_CHART), *COLLECTOR_REACHABLE,
            *COLLECTOR_SECURE, "--set", "autoscaling.enabled=true",
        ],
        capture=True,
    )

    for chart, refusals in ((HEAD_CHART, HEAD_REFUSALS), (COLLECTOR_CHART, COLLECTOR_REFUSALS)):
        for name, overrides, fragment in refusals:
            result = run(
                [helm, "template", "refused", str(chart), *overrides],
                capture=True,
                check=False,
            )
            if result.ok:
                raise ToolFailed(
                    f"The chart rendered {name}, and it must refuse it. "
                    "See docs/DEPLOYMENT.md section 8."
                )
            if fragment not in result.stderr:
                raise ToolFailed(
                    f"The chart refused {name} without naming `{fragment}`. An "
                    "operator has to be told which setting to change."
                )
    say("Every refusal refuses, and names the setting.")
    check_gateway_refusals(helm)

    say("Checking each rendered configuration with the service's own `config check`.")
    with tempfile.TemporaryDirectory(prefix="tallyowl-helm-authority-") as scratch:
        files = _test_authority(head_binary, Path(scratch))
        for chart, release, profile, output in accepted:
            check_config_accepted(output, chart, binaries[chart], files, release, profile)
    return 0
