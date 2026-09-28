"""Tests for the checks `helm-check` makes on a rendered chart.

These exist because of one review finding. The head chart rendered a replicated
cell that could never form a consensus group: each pod named itself from its
pod name, its peers named it from its address, every pod read one shared peer
list that held the pod itself, and the pod advertised 0.0.0.0. Every pod became
ready and no write committed. `helm-check` rendered that cell on every run and
read nothing out of it.

So each check here is given a render that is wrong in one way, and has to
refuse it and say which way. None of these needs Helm: a check reads the text a
render produced.
"""

from __future__ import annotations

import unittest

from tallyowl_tools import helm
from tallyowl_tools.commands import ToolFailed


def cell(
    *,
    peers: str = (
        "cell-0.cell-nodes.telemetry.svc:5200,"
        "cell-1.cell-nodes.telemetry.svc:5200,"
        "cell-2.cell-nodes.telemetry.svc:5200"
    ),
    advertise: str = "$(POD_NAME).cell-nodes.telemetry.svc:5200",
    name: str = "node-$(POD_NAME)-cell-nodes-telemetry-svc-5200",
    pod_name_first: bool = True,
    publish: bool = True,
    min_available: int = 2,
) -> str:
    pod_name = """
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name"""
    return f"""
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: cell
spec:
  minAvailable: {min_available}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: cell
data:
  tallyowl.yaml: |
    replication:
      listen: 0.0.0.0:5200
      peers: {peers}
---
apiVersion: v1
kind: Service
metadata:
  name: cell-nodes
spec:
  clusterIP: None
  publishNotReadyAddresses: {str(publish).lower()}
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: cell
spec:
  replicas: 3
  template:
    spec:
      containers:
        - name: head
          image: example.test/tallyowl:0.2.1
          env:{pod_name if pod_name_first else ""}
            - name: TALLYOWL_REPLICATION__ADVERTISE
              value: "{advertise}"
            - name: TALLYOWL_NODE__NAME
              value: "{name}"{"" if pod_name_first else pod_name}
"""


class ReplicatedIdentity(unittest.TestCase):
    def check(self, rendered: str) -> None:
        helm.check_replicated_identity(rendered, "cell", "telemetry", 3)

    def test_a_cell_whose_nodes_agree_passes(self) -> None:
        self.check(cell())

    def test_the_name_rule_is_the_one_the_head_uses(self) -> None:
        # crates/tallyowl-head/src/cluster.rs: `node-` and the address with each
        # `.` and `:` replaced by `-`.
        self.assertEqual(helm.node_name_for("10.0.0.11:5200"), "node-10-0-0-11-5200")

    def test_a_pod_named_from_its_pod_name_is_refused(self) -> None:
        # The original defect. Peers hash `node-cell-0-cell-nodes-…` and the pod
        # hashes `cell-0`, so no two nodes hold one voter set.
        with self.assertRaisesRegex(ToolFailed, "names itself"):
            self.check(cell(name="$(POD_NAME)"))

    def test_a_bind_address_as_the_advertised_address_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "replication.advertise"):
            self.check(cell(advertise="0.0.0.0:5200"))

    def test_a_bind_address_in_the_peer_list_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "replication.peers"):
            self.check(cell(peers="0.0.0.0:5200,0.0.0.0:5200,0.0.0.0:5200"))

    def test_a_peer_list_that_misses_a_replica_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "replication.peers"):
            self.check(cell(peers="cell-0.cell-nodes.telemetry.svc:5200"))

    def test_an_advertised_address_the_peers_do_not_dial_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "one address"):
            self.check(cell(advertise="$(POD_NAME).cell-nodes.other.svc:5200"))

    def test_a_pod_name_defined_after_its_use_is_refused(self) -> None:
        # The kubelet expands $(VAR) only from a variable defined earlier.
        with self.assertRaisesRegex(ToolFailed, "POD_NAME must come before"):
            self.check(cell(pod_name_first=False))

    def test_a_headless_service_that_hides_unready_pods_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "not\\s+ready"):
            self.check(cell(publish=False))


class DisruptionBudget(unittest.TestCase):
    def test_a_majority_passes(self) -> None:
        helm.check_disruption_budget(cell(min_available=2), 3)

    def test_two_of_five_is_refused(self) -> None:
        # The chart once fixed this at 2, so a drain could take three of five.
        with self.assertRaisesRegex(ToolFailed, "majority is 3"):
            helm.check_disruption_budget(cell(min_available=2), 5)

    def test_no_budget_at_all_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "no disruption budget"):
            helm.check_disruption_budget("kind: ConfigMap\n", 3)


def pod(
    *,
    image: str = "example.test/tallyowl:0.2.1",
    seccomp: bool = True,
    grace: bool = True,
    escalate: str = "false",
    drop: str = "ALL",
    memory: bool = True,
    startup: bool = True,
    key_env: str = """
            - name: SECRET_TALLYOWL_API_KEY
              valueFrom:
                secretKeyRef:
                  name: tallyowl-key
                  key: api-key""",
    api_key: str = "env:SECRET_TALLYOWL_API_KEY",
    collector_listen: str = "0.0.0.0:5100",
    self_observation_endpoint: str = "home-collector:5100",
    replicas: int = 1,
) -> str:
    return f"""
apiVersion: v1
kind: ConfigMap
metadata:
  name: home
data:
  tallyowl.yaml: |
    collector:
      listen: {collector_listen}
      apiKey: "{api_key}"
    metrics:
      selfObservation:
        endpoint: "{self_observation_endpoint}"
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: home
spec:
  replicas: {replicas}
  template:
    spec:
      {"terminationGracePeriodSeconds: 120" if grace else ""}
      securityContext:
        runAsNonRoot: true
        {"seccompProfile: {type: RuntimeDefault}" if seccomp else ""}
      containers:
        - name: head
          image: {image}
          env:{key_env}
            - name: OTHER
              value: x
          {"startupProbe: {httpGet: {path: /livez, port: operational}}" if startup else ""}
          securityContext:
            allowPrivilegeEscalation: {escalate}
            capabilities:
              drop: [{drop}]
          resources:
            requests:
              cpu: "1"
              {"memory: 4Gi" if memory else ""}
"""


class PodHardening(unittest.TestCase):
    def test_a_hardened_pod_passes(self) -> None:
        helm.check_pod_hardening(pod(), "tallyowl")

    def test_each_gap_is_refused_by_name(self) -> None:
        cases = {
            "seccomp": pod(seccomp=False),
            "terminationGracePeriodSeconds": pod(grace=False),
            "escalate privilege": pod(escalate="true"),
            "drop every capability": pod(drop="NET_RAW"),
            "BestEffort": pod(memory=False),
            "startupProbe": pod(startup=False),
        }
        for fragment, rendered in cases.items():
            with self.subTest(fragment), self.assertRaisesRegex(ToolFailed, fragment):
                helm.check_pod_hardening(rendered, "tallyowl")

    def test_an_image_whose_version_can_move_is_refused(self) -> None:
        for image in ("example.test/corndogs:latest", "example.test/corndogs", "registry:5000/corndogs"):
            with self.subTest(image), self.assertRaisesRegex(ToolFailed, "version"):
                helm.check_pod_hardening(pod(image=image), "tallyowl")


class KeySecret(unittest.TestCase):
    def test_a_reference_filled_from_the_secret_passes(self) -> None:
        helm.check_key_secret(pod(), "tallyowl", "tallyowl-key")

    def test_a_setting_that_does_not_name_the_variable_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "env:SECRET_TALLYOWL_API_KEY"):
            helm.check_key_secret(pod(api_key=""), "tallyowl", "tallyowl-key")

    def test_a_reference_nothing_fills_is_refused(self) -> None:
        # The collector chart had no `env` at all, so `env:NAME` named a
        # variable that could not exist and the pod stopped at start.
        with self.assertRaisesRegex(ToolFailed, "points at nothing"):
            helm.check_key_secret(pod(key_env=""), "tallyowl", "tallyowl-key")

    def test_a_key_carried_as_a_value_is_refused(self) -> None:
        literal = """
            - name: SECRET_TALLYOWL_API_KEY
              value: tow_not_a_real_key
              valueFrom:
                secretKeyRef:
                  name: tallyowl-key
                  key: api-key"""
        with self.assertRaisesRegex(ToolFailed, "literal value"):
            helm.check_key_secret(pod(key_env=literal), "tallyowl", "tallyowl-key")


class SelfObservationTarget(unittest.TestCase):
    def test_a_service_address_passes(self) -> None:
        helm.check_self_observation_target(pod())

    def test_the_bind_address_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "0.0.0.0:5100"):
            helm.check_self_observation_target(pod(self_observation_endpoint="0.0.0.0:5100"))

    def test_an_empty_endpoint_falls_back_to_the_bind_address_and_is_refused(self) -> None:
        # `collector.listen` is an address a pod binds. The head used to have
        # the Service name written over it, and now dials its own setting.
        with self.assertRaisesRegex(ToolFailed, "0.0.0.0:5100"):
            helm.check_self_observation_target(pod(self_observation_endpoint=""))


JOB = """
---
apiVersion: batch/v1
kind: Job
metadata:
  name: home-maintenance-1a2b3c4d
spec:
  template:
    spec:
      volumes:
        - name: config
          configMap: {name: home}
        - name: data
          persistentVolumeClaim: {claimName: CLAIM}
"""


class Maintenance(unittest.TestCase):
    def test_a_job_with_the_volume_and_a_stopped_head_passes(self) -> None:
        helm.check_maintenance(pod(replicas=0) + JOB.replace("CLAIM", "data-home-0"), "home")

    def test_a_head_that_still_holds_the_lock_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "lock"):
            helm.check_maintenance(pod(replicas=1) + JOB.replace("CLAIM", "data-home-0"), "home")

    def test_a_job_on_another_volume_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "data-home-0"):
            helm.check_maintenance(pod(replicas=0) + JOB.replace("CLAIM", "scratch"), "home")

    def test_no_job_is_refused(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "rendered no Job"):
            helm.check_maintenance(pod(replicas=0), "home")


SECURED_POD = """
apiVersion: v1
kind: ConfigMap
metadata:
  name: home
data:
  tallyowl.yaml: |
    installation:
      authorities:
      - /etc/tallyowl-authorities/0/ca.crt
      signingCertificate: /etc/tallyowl-signing/tls.crt
      signingKey: file:/etc/tallyowl-signing/tls.key
    tls:
      certificateDirectories:
      - /etc/tallyowl-certificates/1
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: home
spec:
  template:
    spec:
      containers:
        - name: head
          env:
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: TALLYOWL_NODE__NAME
              value: "node-$(POD_NAME)-home-nodes-default-svc-5200"
            - name: TALLYOWL_NODE__FAILURE_DOMAIN
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: SECRET_TALLYOWL_ROLE_TOKEN
              valueFrom:
                secretKeyRef:
                  name: collector-role-token
                  key: token
"""


class PodConfiguration(unittest.TestCase):
    """What `config check` is given must be what the pod starts with."""

    def test_each_mounted_path_is_a_file_on_this_host(self) -> None:
        import pathlib

        files = pathlib.Path("/checked")
        settings, _ = helm.pod_configuration(SECURED_POD, "tallyowl", files, "home")
        self.assertIn("/checked/intermediate.crt", settings)
        self.assertIn("file:/checked/intermediate.key", settings)
        self.assertIn("/checked/root.crt", settings)
        self.assertIn("/checked/collector", settings)
        self.assertNotIn("/etc/tallyowl-", settings)

    def test_the_environment_is_what_the_kubelet_gives_the_first_pod(self) -> None:
        import pathlib

        _, environment = helm.pod_configuration(
            SECURED_POD, "tallyowl", pathlib.Path("/checked"), "home"
        )
        self.assertEqual(environment["POD_NAME"], "home-0")
        self.assertEqual(
            environment["TALLYOWL_NODE__NAME"], "node-home-0-home-nodes-default-svc-5200"
        )
        self.assertEqual(environment["TALLYOWL_NODE__FAILURE_DOMAIN"], "host-a")
        # A reference is parsed and never read, so any value stands in.
        self.assertTrue(environment["SECRET_TALLYOWL_ROLE_TOKEN"])


class ConfigAccepted(unittest.TestCase):
    """The step that runs the service's own `config check` on a render."""

    def service(self, script: str) -> "pathlib.Path":
        import os
        import pathlib
        import tempfile

        directory = pathlib.Path(tempfile.mkdtemp(prefix="tallyowl-helm-test-"))
        self.addCleanup(__import__("shutil").rmtree, directory)
        binary = directory / "tallyowl-head"
        binary.write_text("#!/bin/sh\n" + script)
        os.chmod(binary, 0o755)
        return binary

    def test_a_configuration_the_service_refuses_fails_the_check_and_says_why(self) -> None:
        import pathlib

        binary = self.service(
            'echo "`head.listen` is `0.0.0.0:5110`, which other nodes reach over a network"\nexit 1\n'
        )
        with self.assertRaisesRegex(ToolFailed, "home profile(.|\\n)*head.listen"):
            helm.check_config_accepted(
                SECURED_POD, "tallyowl", binary, pathlib.Path("/checked"), "home", "home"
            )

    def test_a_configuration_the_service_accepts_passes(self) -> None:
        import pathlib

        binary = self.service('[ "$1 $2" = "config check" ] || exit 2\nexit 0\n')
        helm.check_config_accepted(
            SECURED_POD, "tallyowl", binary, pathlib.Path("/checked"), "home", "home"
        )

    def test_a_setting_in_the_environment_of_this_host_never_reaches_the_check(self) -> None:
        import os
        import pathlib

        # A developer's own `TALLYOWL_` variable would otherwise decide the
        # result, and CI would disagree with the workstation.
        os.environ["TALLYOWL_TRANSPORT__ALLOW_PLAINTEXT"] = "true"
        self.addCleanup(os.environ.pop, "TALLYOWL_TRANSPORT__ALLOW_PLAINTEXT")
        binary = self.service('[ -z "$TALLYOWL_TRANSPORT__ALLOW_PLAINTEXT" ] || exit 1\nexit 0\n')
        helm.check_config_accepted(
            SECURED_POD, "tallyowl", binary, pathlib.Path("/checked"), "home", "home"
        )


class RefusalLists(unittest.TestCase):
    def test_a_deployment_key_never_counts_as_a_setting(self) -> None:
        self.assertIn("deployment", helm.DEPLOYMENT_KEYS)

    def test_every_collector_render_that_must_refuse_for_one_reason_names_its_endpoints(self) -> None:
        # A collector chart refuses loopback endpoints first. A refusal case
        # that left them out would be refused for that reason, whatever it was
        # meant to prove.
        endpoint_cases = {"corndogs.endpoint", "head.endpoint"}
        for name, overrides, fragment in helm.COLLECTOR_REFUSALS:
            if fragment in endpoint_cases:
                continue
            with self.subTest(name):
                for flag in helm.COLLECTOR_REACHABLE:
                    self.assertIn(flag, overrides)

    def test_every_render_that_must_refuse_for_another_reason_is_otherwise_secured(self) -> None:
        # D62 made an unsecured render a refusal of its own. A case that left
        # the security values out would be refused for that reason, whatever
        # it was meant to prove.
        d62 = {
            "deployment.tls.signingSecret",
            "dashboard.allowPlaintext",
            "deployment.tls.certificateSecrets",
            "deployment.tls.roleTokenSecret.name",
            "corndogs.endpoint",
            "head.endpoint",
        }
        for refusals, secure in (
            (helm.HEAD_REFUSALS, helm.HEAD_SECURE),
            (helm.COLLECTOR_REFUSALS, helm.COLLECTOR_SECURE),
        ):
            for name, overrides, fragment in refusals:
                if fragment in d62:
                    continue
                with self.subTest(name):
                    for flag in secure:
                        self.assertIn(flag, overrides)


if __name__ == "__main__":
    unittest.main()


def sidecar_pod(head: str, corndogs: str) -> str:
    return f"""kind: StatefulSet
spec:
  template:
    spec:
      containers:
        - name: head
          imagePullPolicy: {head}
        - name: corndogs
          imagePullPolicy: {corndogs}
"""


class SidecarPullPolicy(unittest.TestCase):
    def test_each_container_pulls_by_its_own_policy(self) -> None:
        helm.check_sidecar_pull_policy(sidecar_pod("Never", "IfNotPresent"), "Never", "IfNotPresent")

    def test_a_sidecar_that_took_the_head_policy_is_refused_by_name(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "`corndogs` container pulls with `Never`"):
            helm.check_sidecar_pull_policy(sidecar_pod("Never", "Never"), "Never", "IfNotPresent")

