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
{{- $settings := omit (deepCopy .Values) "image" "replicas" "resources" "persistence" "topologySpread" "affinity" "disruption" "autoscaling" "corndogsDeployment" "securityContext" "bindAddress" "dashboardAssets" "gateway" -}}
{{- $bind := .Values.bindAddress | default "0.0.0.0" -}}
{{- /*
Rewrite the host part of every listening address. A pod that binds loopback is
a pod nothing can reach, and the Service in front of it forwards to a port no
client can use. The port always comes from the setting, so a changed port
reaches the process and the Service together.

An empty address stays empty: `replication.listen` empty means a node with no
replication port, and a host with nothing after it would be a port that opens
because a template was clever.
*/ -}}
{{- range $section, $keys := dict "collector" (list "listen" "operationalListen") "head" (list "listen" "operationalListen") "dashboard" (list "listen") "replication" (list "listen") -}}
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
{{- $settings | toYaml -}}
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
{{- end -}}
