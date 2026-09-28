{{/*
Helpers for the TallyOwl head chart.

The chart refuses a configuration that TallyOwl itself would refuse at start,
because a render failure reaches the operator at `helm install` and a pod
failure reaches them after a deploy. docs/DEPLOYMENT.md section 8 names the
required refusals.
*/}}

{{- define "tallyowl.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "tallyowl.fullname" -}}
{{- printf "%s" .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "tallyowl.labels" -}}
app.kubernetes.io/name: {{ include "tallyowl.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
tallyowl.io/installation: {{ .Values.installation.id | quote }}
tallyowl.io/cell: {{ .Values.cell.id | quote }}
{{- end -}}

{{- define "tallyowl.selectorLabels" -}}
app.kubernetes.io/name: {{ include "tallyowl.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
The TallyOwl settings from the values, with the deployment keys removed.

This is the whole parity story: the rendered configuration IS the values tree,
so a setting added to the values reaches the process without a template change,
and the chart-parity test in crates/tallyowl-config keeps the tree matching the
loader. Keep this list equal to NOT_SETTINGS in
crates/tallyowl-config/tests/chart_parity.rs.
*/}}
{{- define "tallyowl.settings" -}}
{{- $settings := omit (deepCopy .Values) "image" "replicas" "resources" "persistence" "topologySpread" "affinity" "disruption" "autoscaling" "corndogsDeployment" "securityContext" "bindAddress" "dashboardAssets" "gateway" "deployment" -}}
{{- $bind := .Values.bindAddress | default "0.0.0.0" -}}
{{- /*
Rewrite the host part of every listening address. A pod that binds loopback is
a pod nothing can reach, and the Service in front of it forwards to a port no
client can use. The port always comes from the setting, so a changed port
reaches the process and the Service together.

An empty address stays empty: `replication.listen` empty means a node with no
replication port, and a host with nothing after it would be a port that opens
because a template was clever.

Only the listeners a head opens. `collector.listen` stays the loader's loopback
default: a head never binds it, and a network address there would make the
head's own `config check` ask for collector certificates (D62).
*/ -}}
{{- range $section, $keys := dict "head" (list "listen" "operationalListen") "dashboard" (list "listen") "replication" (list "listen") -}}
{{- range $key := $keys -}}
{{- $current := get (get $settings $section) $key -}}
{{- if $current -}}
{{- $_ := set (get $settings $section) $key (printf "%s:%s" $bind (include "tallyowl.port" $current)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $otlp := .Values.compatibility.openTelemetry.listen -}}
{{- if $otlp -}}
{{- $_ := set $settings.compatibility.openTelemetry "listen" (printf "%s:%s" $bind (include "tallyowl.port" $otlp)) -}}
{{- end -}}
{{- /*
The dashboard bundle lives at a fixed path in the image. The setting keeps the
loader default, which is where a developer builds it.
*/ -}}
{{- if .Values.dashboardAssets -}}
{{- $_ := set $settings.dashboard "assets" .Values.dashboardAssets -}}
{{- end -}}
{{- /*
The peers of a first bootstrap. Every pod reads this one ConfigMap, so the list
names every replica, and each node removes its own address from it. A peer
address is always a name another pod can dial: never the bind address.
*/ -}}
{{- if and .Values.replication.listen (not .Values.replication.peers) (gt (int .Values.replicas) 1) -}}
{{- $_ := set $settings.replication "peers" (include "tallyowl.peerList" .) -}}
{{- end -}}
{{- /*
The head does not listen on `collector.listen`. It dials a collector, to send
its own metrics, and a bind address is not an address anything can dial. The
address it dials has its own setting, `metrics.selfObservation.endpoint`, which
takes a DNS name. `collector.listen` stays an address to bind, and TallyOwl
refuses a host name there.
*/ -}}
{{- if .Values.deployment.selfObservationCollector -}}
{{- $_ := set $settings.metrics.selfObservation "endpoint" .Values.deployment.selfObservationCollector -}}
{{- end -}}
{{- if .Values.deployment.apiKeySecret.name -}}
{{- $_ := set $settings.collector "apiKey" "env:SECRET_TALLYOWL_API_KEY" -}}
{{- end -}}
{{- /*
D62. The files of `deployment.tls`, at the paths `tallyowl.securityMounts`
mounts them.
*/ -}}
{{- $tls := .Values.deployment.tls -}}
{{- if $tls.signingSecret -}}
{{- $_ := set $settings.installation "signingCertificate" "/etc/tallyowl-signing/tls.crt" -}}
{{- $_ := set $settings.installation "signingKey" "file:/etc/tallyowl-signing/tls.key" -}}
{{- end -}}
{{- $authorities := $settings.installation.authorities | default list -}}
{{- range $index, $authority := $tls.authorities -}}
{{- $authorities = append $authorities (printf "/etc/tallyowl-authorities/%d/%s" $index ($authority.key | default "ca.crt")) -}}
{{- end -}}
{{- $_ := set $settings.installation "authorities" $authorities -}}
{{- /*
D62. The installation authority signs the certificate Corndogs serves, whether
Corndogs is the sidecar or a shared release from the Corndogs chart, so the
head checks Corndogs against it unless corndogs.tls.caFile names another. With
no authority the head uses the system authorities. A sidecar with no certificate
serves plaintext, which only `transport.allowPlaintext` permits; a CA file there
would make the head speak TLS to it.
A Corndogs sidecar that serves TLS serves it on its one RPC port, so the head
reaches it over TLS too, by the Service name on its certificate.
*/ -}}
{{- $plainSidecar := and .Values.corndogsDeployment.enabled (not .Values.corndogsDeployment.tlsSecret) -}}
{{- if and (not $settings.corndogs.tls.caFile) $authorities (not $plainSidecar) -}}
{{- $_ := set $settings.corndogs.tls "caFile" (first $authorities) -}}
{{- end -}}
{{- if and .Values.corndogsDeployment.enabled .Values.corndogsDeployment.tlsSecret -}}
{{- if not $settings.corndogs.tls.serverName -}}
{{- $_ := set $settings.corndogs.tls "serverName" (printf "%s-corndogs.%s.svc" (include "tallyowl.fullname" .) .Release.Namespace) -}}
{{- end -}}
{{- end -}}
{{- /*
The dashboard serves plaintext and carries session tokens. Behind the Gateway,
which ends TLS, that is the D62 exception.
*/ -}}
{{- if and .Values.gateway.enabled .Values.gateway.dashboard.enabled -}}
{{- $_ := set $settings.dashboard "allowPlaintext" true -}}
{{- end -}}
{{- $settings | toYaml -}}
{{- end -}}

{{/*
The Secret volumes of `deployment.tls`, and where the container mounts them.
The StatefulSet and the maintenance Job use the same two, so a verb reads the
same configuration the head reads. Each file is read-only, with mode 0440: the
pod's `fsGroup` is the group of the process, and nobody else reads a key.
*/}}
{{- define "tallyowl.securityVolumes" -}}
{{- with .Values.deployment.tls.signingSecret }}
- name: tls-signing
  secret:
    secretName: {{ . }}
    defaultMode: 0440
{{- end }}
{{- range $index, $authority := .Values.deployment.tls.authorities }}
- name: tls-authority-{{ $index }}
  secret:
    secretName: {{ required "Each entry of deployment.tls.authorities needs a secretName." $authority.secretName }}
    defaultMode: 0440
{{- end }}
{{- end -}}

{{- define "tallyowl.securityMounts" -}}
{{- if .Values.deployment.tls.signingSecret }}
- name: tls-signing
  mountPath: /etc/tallyowl-signing
  readOnly: true
{{- end }}
{{- range $index, $authority := .Values.deployment.tls.authorities }}
- name: tls-authority-{{ $index }}
  mountPath: /etc/tallyowl-authorities/{{ $index }}
  readOnly: true
{{- end }}
{{- end -}}

{{/*
The domain every pod of this release is reachable in, through the headless
Service. `$(POD_NAME)` in front of it is one pod.
*/}}
{{- define "tallyowl.nodesDomain" -}}
{{- printf "%s-nodes.%s.svc" (include "tallyowl.fullname" .) .Release.Namespace -}}
{{- end -}}

{{/*
One dialable address for each replica, separated by commas.
*/}}
{{- define "tallyowl.peerList" -}}
{{- $port := include "tallyowl.port" .Values.replication.listen -}}
{{- $domain := include "tallyowl.nodesDomain" . -}}
{{- $name := include "tallyowl.fullname" . -}}
{{- $peers := list -}}
{{- range $ordinal := until (int .Values.replicas) -}}
{{- $peers = append $peers (printf "%s-%d.%s:%s" $name $ordinal $domain $port) -}}
{{- end -}}
{{- join "," $peers -}}
{{- end -}}

{{/*
What a node calls a peer, from the peer's address: `node-` and the address with
each `.` and `:` replaced by `-`. This is the rule in
crates/tallyowl-head/src/cluster.rs, and a consensus ID is a hash of this name.
A pod takes the same name for itself, so a node and its peers agree on its ID.
The argument is the address with `$(POD_NAME)` still in it.
*/}}
{{- define "tallyowl.nodeNameFor" -}}
{{- printf "node-%s" (. | replace "." "-" | replace ":" "-") -}}
{{- end -}}

{{/*
A majority of the replicas: floor(replicas / 2) + 1.
*/}}
{{- define "tallyowl.majority" -}}
{{- add (div (int .Values.replicas) 2) 1 -}}
{{- end -}}

{{- define "tallyowl.minAvailable" -}}
{{- if .Values.disruption.minAvailable | toString -}}
{{- .Values.disruption.minAvailable -}}
{{- else -}}
{{- include "tallyowl.majority" . -}}
{{- end -}}
{{- end -}}

{{/*
A duration in seconds, from the forms the loader reads: "30s", "5m", "1h",
"7d". A bare number is seconds.
*/}}
{{- define "tallyowl.durationSeconds" -}}
{{- $text := . | toString | trim -}}
{{- $number := regexFind "^[0-9]+" $text -}}
{{- if not $number -}}
{{- fail (printf "`%s` is not a duration this chart can read. Use a number with s, m, h, or d." $text) -}}
{{- end -}}
{{- $unit := trimPrefix $number $text -}}
{{- if eq $unit "s" -}}{{ $number }}
{{- else if eq $unit "m" -}}{{ mul (atoi $number) 60 }}
{{- else if eq $unit "h" -}}{{ mul (atoi $number) 3600 }}
{{- else if eq $unit "d" -}}{{ mul (atoi $number) 86400 }}
{{- else if eq $unit "" -}}{{ $number }}
{{- else -}}
{{- fail (printf "`%s` is not a duration this chart can read. Use a number with s, m, h, or d." $text) -}}
{{- end -}}
{{- end -}}

{{/*
A size in bytes, from the forms the loader reads: "512KiB", "16MiB", "1GiB",
"64TiB". A bare number is bytes.
*/}}
{{- define "tallyowl.sizeBytes" -}}
{{- $text := . | toString | trim -}}
{{- $number := regexFind "^[0-9]+" $text -}}
{{- if not $number -}}
{{- fail (printf "`%s` is not a size this chart can read. Use a number with KiB, MiB, GiB, or TiB." $text) -}}
{{- end -}}
{{- $unit := trimPrefix $number $text -}}
{{- if eq $unit "KiB" -}}{{ mul (atoi $number) 1024 }}
{{- else if eq $unit "MiB" -}}{{ mul (atoi $number) 1048576 }}
{{- else if eq $unit "GiB" -}}{{ mul (atoi $number) 1073741824 }}
{{- else if eq $unit "TiB" -}}{{ mul (atoi $number) 1099511627776 }}
{{- else if eq $unit "" -}}{{ $number }}
{{- else -}}
{{- fail (printf "`%s` is not a size this chart can read. Use a number with KiB, MiB, GiB, or TiB." $text) -}}
{{- end -}}
{{- end -}}

{{/*
The port part of a listen address such as "127.0.0.1:5110" or "0.0.0.0:5130".
*/}}
{{- define "tallyowl.port" -}}
{{- $parts := splitList ":" (. | toString) -}}
{{- last $parts -}}
{{- end -}}

{{/*
The refusals. Each one names the rule and the document that states it, so the
operator does not have to find the page. TallyOwl refuses the same
configurations at start; the chart refuses them at render, which is earlier.
*/}}
{{- define "tallyowl.validate" -}}
{{- if and (eq .Values.storage.receiptPolicy "local-one") (gt (int .Values.storage.tabletVoters) 1) -}}
{{- fail "storage.receiptPolicy `local-one` is legal only for a tablet with one voter, and storage.tabletVoters is more than one. Use `local-quorum`. See docs/DEPLOYMENT.md section 4 and D27." -}}
{{- end -}}
{{- if and (gt (int .Values.storage.tabletVoters) 1) (not .Values.replication.listen) -}}
{{- fail "storage.tabletVoters is more than one and replication.listen is empty. A node with peers and no address to be reached at elects nothing and refuses every write. Give replication.listen an address. See docs/DEPLOYMENT.md section 4." -}}
{{- end -}}
{{- if and (eq .Values.corndogs.backend "file") (gt (int .Values.corndogs.durableCopies) 1) -}}
{{- fail "corndogs.durableCopies is more than one and the `file` backend holds one copy. Use the `postgres` backend, or set corndogs.durableCopies to 1. See docs/DEPLOYMENT.md section 4 and D4." -}}
{{- end -}}
{{- $grace := include "tallyowl.durationSeconds" .Values.compaction.gcGrace | atoi -}}
{{- $runtime := include "tallyowl.durationSeconds" .Values.query.maxRuntime | atoi -}}
{{- if le $grace $runtime -}}
{{- fail (printf "compaction.gcGrace (%s) must exceed query.maxRuntime (%s), or compaction can remove data that a running query still reads. See docs/FAILURE_MODES.md section 8.1." (.Values.compaction.gcGrace | toString) (.Values.query.maxRuntime | toString)) -}}
{{- end -}}
{{- $dedup := include "tallyowl.durationSeconds" .Values.storage.deduplicationWindow | atoi -}}
{{- $delivery := include "tallyowl.durationSeconds" .Values.corndogs.maxDeliveryAge | atoi -}}
{{- if le $dedup $delivery -}}
{{- fail (printf "storage.deduplicationWindow (%s) must exceed corndogs.maxDeliveryAge (%s), or a retry that outlives the head's memory commits a batch a second time. See D36." (.Values.storage.deduplicationWindow | toString) (.Values.corndogs.maxDeliveryAge | toString)) -}}
{{- end -}}
{{- $split := include "tallyowl.sizeBytes" .Values.placement.splitAbove | atoi -}}
{{- $merge := include "tallyowl.sizeBytes" .Values.placement.mergeBelow | atoi -}}
{{- if ge (mul $merge 2) $split -}}
{{- fail (printf "placement.mergeBelow (%s) must be below half of placement.splitAbove (%s), or a cell rewrites the same data for ever. See docs/CELLS.md section 6." (.Values.placement.mergeBelow | toString) (.Values.placement.splitAbove | toString)) -}}
{{- end -}}
{{- $seal := include "tallyowl.sizeBytes" .Values.collector.maxBatchBytes | atoi -}}
{{- $payload := include "tallyowl.sizeBytes" .Values.corndogs.maxPayloadBytes | atoi -}}
{{- if le $payload $seal -}}
{{- fail (printf "corndogs.maxPayloadBytes (%s) must exceed the batch seal size collector.maxBatchBytes (%s), because a whole batch travels inside one task. See docs/DEPLOYMENT.md section 4." (.Values.corndogs.maxPayloadBytes | toString) (.Values.collector.maxBatchBytes | toString)) -}}
{{- end -}}
{{- if and (gt (int .Values.replicas) 1) (le (int .Values.storage.tabletVoters) 1) -}}
{{- fail "replicas is more than one and storage.tabletVoters is one. The extra pods would hold nothing. Raise storage.tabletVoters with replicas, or lower replicas." -}}
{{- end -}}
{{- range $peer := splitList "," (.Values.replication.peers | toString) -}}
{{- $host := (splitList ":" (trim $peer)) | first -}}
{{- if has $host (list "0.0.0.0" "127.0.0.1" "localhost" "::" "[::]" "[::1]") -}}
{{- fail (printf "replication.peers holds `%s`, which is a bind address or a loopback address. No other node can dial it, so the group never forms. Leave replication.peers empty and the chart writes one address for each replica. See docs/DEPLOYMENT.md section 4." (trim $peer)) -}}
{{- end -}}
{{- end -}}
{{- /*
A node's name goes on its certificate as a DNS name, one label of at most 63
characters, and a replicated pod takes its name from its address. The longest
name is the last pod's.
*/ -}}
{{- if and .Values.replication.listen (gt (int .Values.replicas) 1) -}}
{{- $last := printf "%s-%d.%s:%s" (include "tallyowl.fullname" .) (sub (int .Values.replicas) 1) (include "tallyowl.nodesDomain" .) (include "tallyowl.port" .Values.replication.listen) -}}
{{- $name := include "tallyowl.nodeNameFor" $last -}}
{{- if gt (len $name) 63 -}}
{{- fail (printf "The node name `%s` has %d characters, and a node name goes on the node's certificate as one DNS label of at most 63 (D62). The name comes from the release name and the namespace. Use a shorter release name or namespace." $name (len $name)) -}}
{{- end -}}
{{- end -}}
{{- if and .Values.disruption.enabled (gt (int .Values.replicas) 1) (.Values.disruption.minAvailable | toString) -}}
{{- if lt (int .Values.disruption.minAvailable) (int (include "tallyowl.majority" .)) -}}
{{- fail (printf "disruption.minAvailable is %v and a majority of %v replicas is %s. A lower number lets one node drain end a quorum. Leave disruption.minAvailable empty for a majority, or set it to %s or more. See docs/DEPLOYMENT.md section 6." .Values.disruption.minAvailable .Values.replicas (include "tallyowl.majority" .) (include "tallyowl.majority" .)) -}}
{{- end -}}
{{- end -}}
{{- if .Values.corndogsDeployment.enabled -}}
{{- if not .Values.persistence.enabled -}}
{{- fail "corndogsDeployment.enabled is true and persistence.enabled is false. The queue would be on the container file system, and a pod restart would delete batches that a collector called durable. Set persistence.enabled to true, or use a Corndogs service that has its own volume." -}}
{{- end -}}
{{- $tag := (splitList ":" (.Values.corndogsDeployment.image | toString)) | last -}}
{{- if or (eq $tag "latest") (not (contains ":" (.Values.corndogsDeployment.image | toString))) -}}
{{- fail "corndogsDeployment.image has the tag `latest` or no tag. The queue carries every receipt, and its version must not change when a pod moves. Give the image a version tag, for example corndogs:0.7.6." -}}
{{- end -}}
{{- if and (not .Values.corndogsDeployment.tlsSecret) (not .Values.transport.allowPlaintext) -}}
{{- fail "corndogsDeployment.enabled is true and corndogsDeployment.tlsSecret is empty. Collectors reach this Corndogs over the pod network, and D62 puts TLS on that hop. Make a certificate for the names `<release>-corndogs` and `<release>-corndogs.<namespace>.svc`, signed by the installation authority, store it in a kubernetes.io/tls Secret, and name it in corndogsDeployment.tlsSecret. Or set transport.allowPlaintext=true if something else protects this network. See docs/DEPLOYMENT.md section 7c." -}}
{{- end -}}
{{- if and .Values.corndogsDeployment.tlsSecret (not (or .Values.deployment.tls.authorities .Values.corndogs.tls.caFile)) -}}
{{- fail "corndogsDeployment.tlsSecret is set and the head has no authority to check it with. Name the installation authority in deployment.tls.authorities, or set corndogs.tls.caFile. See docs/DEPLOYMENT.md section 7c." -}}
{{- end -}}
{{- end -}}
{{- if and .Values.metrics.selfObservation.enabled (not .Values.deployment.selfObservationCollector) (not .Values.metrics.selfObservation.endpoint) -}}
{{- fail "metrics.selfObservation.enabled is true and deployment.selfObservationCollector is empty. The head sends its own metrics to a collector, and in a pod it has no address for one. Set deployment.selfObservationCollector to the collector Service, for example tallyowl-collector:5100, and name the key Secret in deployment.apiKeySecret.name." -}}
{{- end -}}
{{- /*
D62. In a pod `head.listen` is reached over the pod network, so the head needs
an authority to sign with and the authorities to verify against, or an explicit
statement that something else protects the network.
*/ -}}
{{- $signer := or .Values.deployment.tls.signingSecret (and .Values.installation.signingCertificate .Values.installation.signingKey) -}}
{{- $trusted := or .Values.deployment.tls.authorities .Values.installation.authorities -}}
{{- if and (not .Values.transport.allowPlaintext) (not (and $signer $trusted)) -}}
{{- fail "head.listen is reached over the pod network, so it uses mutual TLS (D62), and the head has no authority to sign with or none to trust. Make one with `tallyowl-head ca create <directory>`, store it in Secrets, and name them in deployment.tls.signingSecret and deployment.tls.authorities. Or set transport.allowPlaintext=true if something else protects this network. See docs/DEPLOYMENT.md section 7c." -}}
{{- end -}}
{{- $routed := and .Values.gateway.enabled .Values.gateway.dashboard.enabled -}}
{{- if and .Values.dashboard.enabled (not .Values.dashboard.allowPlaintext) (not $routed) -}}
{{- fail "dashboard.enabled is true, and in a pod the dashboard is reached over the network. It serves plaintext and carries session tokens (D62). Publish it through a Gateway that ends TLS (gateway.enabled=true and gateway.parentRef.name), or set dashboard.allowPlaintext=true if something else protects the network, or set dashboard.enabled=false. See docs/DEPLOYMENT.md section 7c." -}}
{{- end -}}
{{- if and .Values.deployment.maintenance.enabled (not .Values.persistence.enabled) -}}
{{- fail "deployment.maintenance.enabled is true and persistence.enabled is false. There is no volume for the Job to open: with persistence off, the data is gone when the head pod stops." -}}
{{- end -}}
{{- if and .Values.deployment.maintenance.enabled (not .Values.deployment.maintenance.args) (not .Values.deployment.maintenance.command) -}}
{{- fail "deployment.maintenance.enabled is true and deployment.maintenance.args is empty. Give the verb and its arguments, for example --set 'deployment.maintenance.args={provision,shop}'. See docs/DEPLOYMENT.md section 9." -}}
{{- end -}}
{{- end -}}
