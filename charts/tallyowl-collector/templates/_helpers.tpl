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
{{- $settings := omit (deepCopy .Values) "image" "replicas" "resources" "persistence" "topologySpread" "affinity" "disruption" "autoscaling" "corndogsDeployment" "securityContext" "bindAddress" "dashboardAssets" "gateway" "deployment" -}}
{{- $bind := .Values.bindAddress | default "0.0.0.0" -}}
{{- /*
Rewrite the host part of every listening address. A pod that binds loopback is
a pod nothing can reach, and the Service in front of it forwards to a port no
client can use. The port always comes from the setting, so a changed port
reaches the process and the Service together.

Only the listeners a collector opens. `head.listen`, `dashboard.listen`, and
`replication.listen` stay the loader's loopback defaults: a collector binds
none of them, and a network address there would make the collector's own
`config check` ask for a head's identity (D62).
*/ -}}
{{- range $section, $keys := dict "collector" (list "listen" "operationalListen") -}}
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
{{- if .Values.deployment.apiKeySecret.name -}}
{{- $_ := set $settings.collector "apiKey" "env:SECRET_TALLYOWL_API_KEY" -}}
{{- end -}}
{{- /*
D62. The files and the token of `deployment.tls`, where the Deployment mounts
them.
*/ -}}
{{- $tls := .Values.deployment.tls -}}
{{- $directories := $settings.tls.certificateDirectories | default list -}}
{{- range $index, $name := $tls.certificateSecrets -}}
{{- $directories = append $directories (printf "/etc/tallyowl-certificates/%d" $index) -}}
{{- end -}}
{{- $_ := set $settings.tls "certificateDirectories" $directories -}}
{{- $authorities := $settings.installation.authorities | default list -}}
{{- range $index, $authority := $tls.authorities -}}
{{- $authorities = append $authorities (printf "/etc/tallyowl-authorities/%d/%s" $index ($authority.key | default "ca.crt")) -}}
{{- end -}}
{{- $_ := set $settings.installation "authorities" $authorities -}}
{{- if $tls.roleTokenSecret.name -}}
{{- $_ := set $settings.enrollment "roleToken" "env:SECRET_TALLYOWL_ROLE_TOKEN" -}}
{{- end -}}
{{- /*
D62. In a pod the queue is always a Service, reached over TLS. Its certificate
is signed by the installation authority, so the collector trusts that one for
it unless `corndogs.tls.caFile` names another. The server name is the host of
`corndogs.endpoint`.
*/ -}}
{{- if and (not $settings.corndogs.tls.caFile) $authorities -}}
{{- $_ := set $settings.corndogs.tls "caFile" (first $authorities) -}}
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
{{- range $setting := list "corndogs.endpoint" "head.endpoint" -}}
{{- $parts := splitList "." $setting -}}
{{- $value := get (get $.Values (first $parts)) (last $parts) | toString -}}
{{- $host := (splitList ":" $value) | first -}}
{{- if or (not $value) (has $host (list "127.0.0.1" "localhost" "0.0.0.0" "::1" "[::1]")) -}}
{{- fail (printf "%s is `%s`, and in a collector pod that address reaches nothing. The collector stops at start when it cannot reach the queue. Set corndogs.endpoint to the queue Service, for example <head release>-corndogs:5080, and head.endpoint to the head Service, for example <head release>:5110. See docs/DEPLOYMENT.md section 3a." $setting $value) -}}
{{- end -}}
{{- end -}}
{{- /*
D62. In a pod, applications reach intake over the pod network, and the
collector reaches the head over it.
*/ -}}
{{- if not .Values.transport.allowPlaintext -}}
{{- if not (or .Values.deployment.tls.certificateSecrets .Values.tls.certificateDirectories) -}}
{{- fail "collector.listen is reached over the pod network, so applications reach it with TLS (D62), and deployment.tls.certificateSecrets is empty. Store the collector certificate in a Secret of type kubernetes.io/tls and name it in deployment.tls.certificateSecrets. Or set transport.allowPlaintext=true if something else protects this network. See docs/DEPLOYMENT.md section 7c." -}}
{{- end -}}
{{- $trusted := or .Values.deployment.tls.authorities .Values.installation.authorities -}}
{{- $token := or .Values.deployment.tls.roleTokenSecret.name .Values.enrollment.roleToken -}}
{{- if not (and $trusted $token) -}}
{{- fail "head.endpoint is reached over the pod network, so the collector reaches the head over mutual TLS (D62), and it needs the authorities to verify the head against and a role token to enroll with. Name them in deployment.tls.authorities and deployment.tls.roleTokenSecret.name. Or set transport.allowPlaintext=true if something else protects this network. See docs/DEPLOYMENT.md section 7c." -}}
{{- end -}}
{{- end -}}
{{- if .Values.autoscaling.enabled -}}
{{- if not (((.Values.resources).requests).cpu) -}}
{{- fail "autoscaling.enabled is true and resources.requests.cpu is empty. The autoscaler measures CPU against the request, so with no request it never scales. Set resources.requests.cpu." -}}
{{- end -}}
{{- end -}}
{{- $needsKey := or .Values.compatibility.openTelemetry.enabled .Values.compatibility.prometheus.targets .Values.metrics.selfObservation.enabled -}}
{{- if and $needsKey (not .Values.deployment.apiKeySecret.name) (not .Values.collector.apiKey) -}}
{{- fail "A compatibility receiver or self-observation is on, and the collector has no key to present. Make a key, store it in a Secret, and set deployment.apiKeySecret.name. See docs/DEPLOYMENT.md section 3a." -}}
{{- end -}}
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
