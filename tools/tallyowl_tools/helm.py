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

#: Each refusal the charts must make at render, from docs/DEPLOYMENT.md
#: sections 4 and 8. The fragment is what the failure message must name, so a
#: refusal that fires for the wrong reason still fails this check.
HEAD_REFUSALS: list[tuple[str, list[str], str]] = [
    (
        "local-one with three voters",
        ["--set", "storage.tabletVoters=3", "--set", "replication.listen=0.0.0.0:5200"],
        "local-one",
    ),
    (
        "three voters with no replication address",
        ["--set", "storage.tabletVoters=3", "--set", "storage.receiptPolicy=local-quorum"],
        "replication.listen",
    ),
    (
        "two durable copies on the file backend",
        ["--set", "corndogs.durableCopies=2"],
        "durableCopies",
    ),
    (
        "a gcGrace shorter than the longest query",
        ["--set", "compaction.gcGrace=10s"],
        "gcGrace",
    ),
    (
        "a merge threshold that is not below half the split threshold",
        ["--set", "placement.mergeBelow=32GiB"],
        "mergeBelow",
    ),
    (
        "a deduplication window the retry window outlives",
        ["--set", "storage.deduplicationWindow=12h"],
        "deduplicationWindow",
    ),
    (
        "three replicas with one tablet voter",
        ["--set", "replicas=3"],
        "tabletVoters",
    ),
]

COLLECTOR_REFUSALS: list[tuple[str, list[str], str]] = [
    (
        "a batch seal the queue payload cannot carry",
        ["--set", "corndogs.maxPayloadBytes=256KiB"],
        "maxPayloadBytes",
    ),
    (
        "two durable copies on the file backend",
        ["--set", "corndogs.durableCopies=2"],
        "durableCopies",
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
)


#: What a rendered pod must bind, and where it must find the dashboard. A
#: loopback bind in a pod reaches nothing and the Service in front of it
#: forwards to a port no client can use, which is a deployment that looks
#: healthy and serves nobody.
EXPECTED_BINDS = {
    "collector": ("listen", "operationalListen"),
    "head": ("listen", "operationalListen"),
    "dashboard": ("listen",),
}


def check_rendered_binds(rendered: str, chart: str) -> None:
    """Every listening address in the rendered settings faces the network."""
    import yaml

    settings = _rendered_settings(rendered, chart)
    for section, keys in EXPECTED_BINDS.items():
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


GATEWAY_REFUSALS = (
    (
        "a route with no Gateway to attach to",
        ["--set", "gateway.enabled=true"],
        "gateway.parentRef.name",
    ),
    (
        "a dashboard route with the dashboard turned off",
        [
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
    helm = deps.helm_program()

    for chart in (HEAD_CHART, COLLECTOR_CHART):
        run([helm, "lint", str(chart)], quiet=True)

    say("Rendering the home profile, which is the default values.")
    head = run([helm, "template", "home", str(HEAD_CHART)], capture=True)
    collector = run([helm, "template", "home", str(COLLECTOR_CHART)], capture=True)

    check_rendered_settings(head.stdout, "tallyowl")
    check_rendered_settings(collector.stdout, "tallyowl-collector")
    check_rendered_binds(head.stdout, "tallyowl")
    check_rendered_binds(collector.stdout, "tallyowl-collector")

    say("Rendering the Gateway API routes.")
    for chart, overrides in (
        (HEAD_CHART, ["--set", "gateway.enabled=true", "--set", "gateway.parentRef.name=platform"]),
        (
            COLLECTOR_CHART,
            [
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

    say("Rendering one replicated cell.")
    run([helm, "template", "cell", str(HEAD_CHART), *REPLICATED], capture=True)

    say("Rendering the collector with autoscaling on.")
    run(
        [
            helm, "template", "scaled", str(COLLECTOR_CHART),
            "--set", "autoscaling.enabled=true",
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
    return 0
