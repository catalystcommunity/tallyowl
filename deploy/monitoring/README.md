# Monitoring TallyOwl itself

This directory holds the alert rules for a TallyOwl installation. These rules
are about TallyOwl. [ALERTS.md](../../docs/ALERTS.md) is about alerts on your
own telemetry.

`tallyowl-rules.yaml` uses the Prometheus rule file format. The head and each
collector publish their instruments at `/metrics` on the operational port.
The format is the Prometheus text format.

## 1. Collect the instruments

Do one of these.

- Set `deployment.podAnnotations` in each chart to the scrape annotations that
  your scraper reads.
- Set `deployment.serviceMonitor.enabled` to `true` in each chart. This needs
  the ServiceMonitor custom resource in the cluster.
- On a host with no cluster, add the two operational addresses as static
  targets: `head.operationalListen` and `collector.operationalListen`.

## 2. Load the rules

Give `tallyowl-rules.yaml` to your rule evaluator as a rule file. For a cluster
that uses PrometheusRule objects, put the content of `groups` below `spec` of
one PrometheusRule object.

## 3. What each rule means

| Rule | Meaning | First action |
| --- | --- | --- |
| `TallyOwlRetrySweepStopped` | The timeout sweep did not run for five minutes. Nothing retries | Check the connection from the collector to Corndogs |
| `TallyOwlDeliveryStopped` | Batches wait and none reaches the head | Check the head, then restart the collector if the head is correct |
| `TallyOwlDeliveryQueueGrowing` | The queue grows for 30 minutes | Restore the head before `corndogs.maxDeliveryAge` |
| `TallyOwlBatchesQuarantined` | No retry can deliver a batch | Read the batch in the quarantine queue |
| `TallyOwlProtocolVersionRefused` | A sender is older than the compatibility window | Upgrade that sender |
| `TallyOwlAppendLogStalled` | A commit is in flight and the durable position does not move | RUNBOOK_INCIDENT.md section 2 |
| `TallyOwlStorageRefusedAWrite` | The device has too little space | RUNBOOK_INCIDENT.md section 4 |
| `TallyOwlDiskNearTheReserve` | Free space is less than two times the reserve | Grow the volume or shorten the retention |
| `TallyOwlIntegrityFailure`, `TallyOwlSegmentsDamaged` | Stored data failed a checksum | RUNBOOK_INCIDENT.md section 7 |
| `TallyOwlGenerationPinIsOld` | A query holds storage that compaction wants | Find the query. Restart the head if the pin does not end |
| `TallyOwlWorkflowQuarantined`, `TallyOwlWorkflowLagging` | Head alerting or a projector pass is stopped or late | Read the head log |
| `TallyOwlAlertsDisabledByBudget` | The head disabled one of your alert rules | ALERTS.md |
| `TallyOwlScrapeFailures` | A scrape target does not answer | Read the collector log for the target |
| `TallyOwlMetricSeriesRefused` | A metric series budget is full | DATA_MODEL.md section 3.4 |
| `TallyOwlDeliveryLoopStopped` | The delivery loop of a collector does not turn | Restart the collector. Then find the peer that did not answer |
| `TallyOwlBackgroundLoopStale` | A background loop of the head did not finish a pass for ten minutes | Read the `loop` label and the head log |
| `TallyOwlTaskPanicked` | A task stopped on a defect, and the service contained it | Report the log line |
| `TallyOwlSealFailing` | The head cannot write a segment | Check the free space and the device |
| `TallyOwlConsensusGroupStopped` | A consensus group stopped on a node | FAILURE_MODES.md section 6 |
| `TallyOwlConsensusGroupLeaderless` | A consensus group has no leader, and its writes do not commit | Check that a majority of the voters run |
| `TallyOwlReplicaLagging` | A replica stays behind its leader | Check the network and the disk of that replica |
| `TallyOwlApplicationCertificateExpiring`, `TallyOwlApplicationCertificateCritical` | The collector certificate that applications verify expires in less than 14 days, or 3 days | Add a new certificate in a second directory. RUNBOOK_INCIDENT.md section 8.1 |
| `TallyOwlCertificateReloadFailing` | A changed certificate file could not be used, and the collector keeps the old one | RUNBOOK_INCIDENT.md section 8.2 |
| `TallyOwlNodeCertificateNotRenewing` | A node certificate stays below one third of its lifetime, so renewal fails | RUNBOOK_INCIDENT.md section 8.3 |
| `TallyOwlNodeCertificateLapsing` | A node certificate expires in less than one hour | RUNBOOK_INCIDENT.md section 8.3 |
| `TallyOwlEnrollmentFailing` | Enrollment or renewal fails, by reason | RUNBOOK_INCIDENT.md section 8.3 |
| `TallyOwlHandshakesRefused` | A listener refuses TLS handshakes at a steady rate | Read the error at the client. RUNBOOK_INCIDENT.md section 8.4 |

`TallyOwlNodeCertificateNotRenewing` uses 8 hours, which is one third of the
default lifetime of 24 hours. A node renews when one third of the lifetime is
left, and the gauge then goes up again. A value that stays below one third
thus means that renewal fails, and the node has one third of the lifetime
before it stops. If you change `enrollment.certificateLifetimeHours`, change
the threshold to that value divided by 3.

Also alert on the `up` series that your scraper makes for each TallyOwl target.
A service that does not answer its operational port publishes nothing, and no
rule above can fire for it.

## 4. What these rules cannot see

- **Liveness finds no stall.** `/livez` fails only when a role thread of the
  collector ends. See CONVENTIONS.md section 3. `TallyOwlAppendLogStalled`,
  `TallyOwlDeliveryLoopStopped`, and `TallyOwlBackgroundLoopStale` are the
  rules that find a stall.
- **A head with no queue.** The `workflow-queue` health check fails until the
  head reaches Corndogs, thus the head is not ready. No rule here reads a
  health check. Alert on readiness in your cluster.
- **One consensus group.** The consensus gauges are sums for one node and have
  no group label. The head log names a stopped group.
- **A refused TLS handshake at a server.** A server does not log or count a
  handshake that it refuses. The client reports it. RUNBOOK_INCIDENT.md
  section 8.4.
- **The cause of an enrollment failure.** The node counts it by reason and does
  not log the message.
- **`tallyowl_delivery_oldest_waiting_ms`.** In releases up to 0.2.1 this gauge
  increases under load and does not decrease when the oldest batch commits.
  There is no rule on it for that reason.
- **Counters that do not move.** Some registered counters have no code path
  that changes them in releases up to 0.2.1. FAILURE_MODES.md section 13 gives
  the list. Make the failure occur in a test installation before you rely on a
  rule.
