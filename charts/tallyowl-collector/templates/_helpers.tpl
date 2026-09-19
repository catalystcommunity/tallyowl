{{/*
Helpers for the TallyOwl collector chart.

A collector holds no durable state, so it is a Deployment that can scale
horizontally. The refusals mirror the head chart's where the settings overlap,
because both render the same configuration tree.
*/}}

{{- define "tallyowl-collector.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "tallyowl-collector.fullname" -}}
{{- printf "%s" .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "tallyowl-collector.labels" -}}
app.kubernetes.io/name: {{ include "tallyowl-collector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
tallyowl.io/installation: {{ .Values.installation.id | quote }}
tallyowl.io/cell: {{ .Values.cell.id | quote }}
{{- end -}}

{{- define "tallyowl-collector.selectorLabels" -}}
app.kubernetes.io/name: {{ include "tallyowl-collector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
The TallyOwl settings from the values, with the deployment keys removed. Keep
this list equal to NOT_SETTINGS in
crates/tallyowl-config/tests/chart_parity.rs.
*/}}
{{- define "tallyowl-collector.settings" -}}
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
{{- $_ := set (get $settings $section) $key (printf "%s:%s" $bind (include "tallyowl-collector.port" $current)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $otlp := .Values.compatibility.openTelemetry.listen -}}
{{- if $otlp -}}
{{- $_ := set $settings.compatibility.openTelemetry "listen" (printf "%s:%s" $bind (include "tallyowl-collector.port" $otlp)) -}}
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

{{- define "tallyowl-collector.durationSeconds" -}}
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

{{- define "tallyowl-collector.sizeBytes" -}}
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

{{- define "tallyowl-collector.port" -}}
{{- $parts := splitList ":" (. | toString) -}}
{{- last $parts -}}
{{- end -}}

{{- define "tallyowl-collector.validate" -}}
{{- if and (eq .Values.corndogs.backend "file") (gt (int .Values.corndogs.durableCopies) 1) -}}
{{- fail "corndogs.durableCopies is more than one and the `file` backend holds one copy. Use the `postgres` backend, or set corndogs.durableCopies to 1. See docs/DEPLOYMENT.md section 4 and D4." -}}
{{- end -}}
{{- $seal := include "tallyowl-collector.sizeBytes" .Values.collector.maxBatchBytes | atoi -}}
{{- $payload := include "tallyowl-collector.sizeBytes" .Values.corndogs.maxPayloadBytes | atoi -}}
{{- if le $payload $seal -}}
{{- fail (printf "corndogs.maxPayloadBytes (%s) must exceed the batch seal size collector.maxBatchBytes (%s), because a whole batch travels inside one task. See docs/DEPLOYMENT.md section 4." (.Values.corndogs.maxPayloadBytes | toString) (.Values.collector.maxBatchBytes | toString)) -}}
{{- end -}}
{{- $dedup := include "tallyowl-collector.durationSeconds" .Values.storage.deduplicationWindow | atoi -}}
{{- $delivery := include "tallyowl-collector.durationSeconds" .Values.corndogs.maxDeliveryAge | atoi -}}
{{- if le $dedup $delivery -}}
{{- fail (printf "storage.deduplicationWindow (%s) must exceed corndogs.maxDeliveryAge (%s), or a retry that outlives the head's memory commits a batch a second time. See D36." (.Values.storage.deduplicationWindow | toString) (.Values.corndogs.maxDeliveryAge | toString)) -}}
{{- end -}}
{{- end -}}
