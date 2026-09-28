# Operations runbook

This runbook tells an operator how to run a TallyOwl installation day to day.
[RUNBOOK_INCIDENT.md](RUNBOOK_INCIDENT.md) covers the moments when something
is wrong.

## 1. The processes

A home installation runs three processes: one Corndogs, one head, and one
collector. A replicated cell adds head nodes as storage voters. The charts in
`charts/` install both shapes. See [DEPLOYMENT.md](DEPLOYMENT.md).

| Process | Data port | Health and metrics port |
| --- | --- | --- |
| Corndogs | 5080 | — |
| Head | 5110 | 5111 |
| Collector | 5100 | 5101 |
| Dashboard | 5120 | — |

The health and metrics port serves three paths: `/livez`, `/readyz`, and
`/metrics`. It serves nothing else.

## 2. Daily checks

1. Read `/readyz` on the head and on each collector. A ready process answers
   200.
2. Read `/metrics` and compare these values with yesterday:
   - `tallyowl_delivery_queue_depth_count` — batches that wait for delivery.
     A small number is normal. A number that grows for hours is not.
   - `tallyowl_delivery_oldest_waiting_ms` — how long the oldest batch has
     waited. Zero beside a positive depth means a collector restart lost the
     age. The next claim finds it again.
   - `tallyowl_batches_quarantined_total` — batches that need a person.
     Section 5 says what to do.
   - `tallyowl_integrity_failures_total` — must stay zero.
   - `tallyowl_storage_refusals_total` — writes the store refused, by cause.
   - `tallyowl_tls_certificate_expiry_seconds` — the time until the
     certificate that applications verify expires. Replace it before 14 days.
     Section 3.7.
   - `tallyowl_node_certificate_expiry_seconds` — the time until the node
     certificate of this process expires. It must stay above one third of
     `enrollment.certificateLifetimeHours`. RUNBOOK_INCIDENT.md section 8.3.
3. Read the disk use of the data directory and of the Corndogs directory.

## 3. Keys, sessions, and members

### 3.1 Stop the head before you run these commands

Every `tallyowl-head` verb in this section opens the data directory. One
process owns one data directory. Thus, you must stop the head before you run a
verb. While the head is stopped, queries and the dashboard do not answer.
Collectors continue to accept telemetry, and the durable queue holds it until
the head starts again.

If the head runs, the verb stops with this message: "The data directory could
not be opened." Stop the head, then run the verb again.

In a chart installation, the maintenance Job does these steps. See
[DEPLOYMENT.md](DEPLOYMENT.md).

This release has no control operation that makes a key, a project, or a member
on a running head. That work is not built.

### 3.2 Make a project and its first key

```sh
tallyowl-head --config <file> provision <project> [workspace]
```

1. Give the project name.
2. Give the workspace name if you want a workspace other than `default`. The
   command makes the workspace if it is missing. This is the only command that
   makes a workspace.
3. Store the key as a secret. The command prints the key one time.
4. Give the collector a reference to the key (`file:` or `env:`). Do not give
   the collector the value.

Each operator who is signed in becomes an owner of a new workspace.

### 3.3 Rotate or revoke a key

Keep more than one active key for each source. This permits rotation without
a coordinated cutover.

1. Run `provision` again with the same project and workspace. The command
   prints a second key for the same source.
2. Give the new key to the application.
3. Run `tallyowl-head key list` to find the identifier of the old key.
4. Run `tallyowl-head key revoke <key-id>`.

A revocation reaches collectors within `collector.keyCacheTtl`. If a key has
leaked, stop the head, revoke the key, and start the head. The outage is the
cost of the revocation in this release.

### 3.4 Sign in an operator

```sh
tallyowl-head --config <file> session create <name>
```

The command prints a session token one time. The session is an owner of each
workspace that exists at that time. If no workspace exists, the command tells
you to run `provision` first.

To use the token in the dashboard, open the dashboard and paste the token into
the field "Or paste a session token". A session ends after 24 hours. The
dashboard then shows "Your session ended. Sign in again."

Use `session list` to see the sessions. Use `session revoke <id>` to end one.

### 3.5 Give a teammate a role

A person who signs in through LinkKeys has no role. That person sees nothing
until an operator gives a role.

1. Tell the person to sign in one time.
2. Run `tallyowl-head session list`. Find the subject of the person. The
   subject is the account identifier that LinkKeys gave.
3. Run this command:

   ```sh
   tallyowl-head --config <file> member add <workspace> <subject> <role>
   ```

The roles are `viewer`, `admin`, and `owner`. A viewer reads. An admin also
changes settings, alert rules, and policy. An owner also erases data and
manages role tokens for the installation.

The role applies to the next request of the person. The person does not sign
in again.

Use `member list <workspace>` to see who has a role. Use
`member remove <workspace> <subject>` to remove a role.

### 3.6 Set up transport security for the first time

The head does not need to stop for these steps. DEPLOYMENT.md section 7c gives
the design and the chart values.

1. Make the authorities. The command needs no configuration and no data
   directory:

   ```sh
   tallyowl-head ca create <directory>
   ```

   The command prints the settings for each head and each collector.
2. Move `root.key` to a place that no TallyOwl host can read.
3. On each head, set `installation.authorities`,
   `installation.signingCertificate`, and `installation.signingKey`.
4. On each collector, set `installation.authorities`, and set
   `tls.certificateDirectories` to a directory that holds `tls.crt` and
   `tls.key`. Make that certificate from your own authority, for the names
   that applications dial.
5. Make a role token for the collectors (section 3.8), and set
   `enrollment.roleToken` on each collector to a `file:` or `env:` reference to
   it.
6. Run `tallyowl-head config check` and `tallyowl-collector config check`.
   Then restart each service.

A process whose connections are all on loopback or a unix socket needs none of
this. The home profile on one host is one of those.

### 3.7 Replace a certificate or an authority

**A certificate for applications.** Put the new certificate in a second
directory, and add the directory to `tls.certificateDirectories`. The
collector reads its files every `tls.reloadInterval` and presents the newest
certificate that is valid now. Remove the old directory after the old
certificate expires. No connection stops.

**A node certificate.** Nothing to do. Each node renews its own certificate at
two thirds of its lifetime.

**The authority.**

1. Make a new root and intermediate with `tallyowl-head ca create` in a new
   directory.
2. Add the new root to `installation.authorities` on every head and every
   collector. Keep the old root in the list.
3. Change the signing settings on every head to the new intermediate, and
   restart the heads.
4. Wait for one certificate lifetime.
5. Remove the old root from `installation.authorities`.

### 3.8 Make or revoke a role token

A collector enrolls with a role token.

`tallyowl-head token create` makes a role token. Like each administration
verb, it needs the data directory, so stop the head first. On Kubernetes, run
the verb in the maintenance Job (section 9):

```sh
tallyowl-head token create collectors collector-intake,collector-forwarder shop
```

- The first word after `create` is a label that says what the token is for.
- The second is the roles that the token can enroll, separated by commas. A
  collector uses `collector-intake`, `collector-forwarder`, or both.
- Each word after that is a workspace. A collector that enrolls with the token
  can write only to those workspaces. With no workspace, it can write to all
  of them.

The command prints the token one time. Put it in the Secret that
`enrollment.roleToken` names. The token permits 120 enrollments in each hour.
This is enough for an autoscaler and for a pod that restarts in a loop.

On Kubernetes, the token is in the log of the maintenance Job. Copy it, and
then delete the Job, so that the log does not keep the token.

`tallyowl-head token list` shows the tokens. `tallyowl-head token revoke
<token-id>` stops a token. The nodes that it enrolled cannot renew, and each
one stops when its certificate expires.

A collector that holds a certificate continues until the certificate expires,
and then cannot enroll again. The lifetime is 24 hours by default.

## 4. Backups

Take a snapshot on a schedule:

```sh
tallyowl-head snapshot <directory>
```

Stop the head first. The command copies the stored files and the erasure
records. Run it again into the same directory to add only what is new.

Test the restore. A restore that nobody has run is not a backup. The drill
does the whole cycle and measures it:

```sh
./tools.sh drill dr
```

The drill reports the backup time, the restore time, and what a disaster
loses: everything committed after the snapshot that the delivery queue no
longer held. Choose the snapshot period together with the queue's
`corndogs.maxDeliveryAge`, because the queue is what recovers the gap.

## 5. The quarantine

A batch goes to quarantine when no retry can fix it: a payload that does not
decode, a permanent rejection, or an age past `corndogs.maxDeliveryAge`. The
dashboard's workflow table shows the count, and
`tallyowl_batches_quarantined_total` counts it.

Read the quarantine reason first. A quarantined batch holds safe metadata and
the reason it stopped. Replay it only when the cause is fixed, and know that a
replay after `storage.deduplicationWindow` commits a second logical batch.

## 6. Upgrades

Upgrade in this order. See [DEPLOYMENT.md](DEPLOYMENT.md) section 7.

1. Head ingest and query roles.
2. Storage nodes, one failure domain at a time.
3. Collectors, last.

Each step is a graceful restart. Head ingest stops readiness before it
drains. A collector finishes or releases its claimed tasks and leaves queued
data durable. A collector enrolls again when it starts, so its role token must
be valid at the time of the upgrade. The soak harness proves the procedure
under live load:

```sh
./tools.sh soak roll
```

## 7. Scaling

A collector is stateless and scales horizontally. The collector chart carries
a `HorizontalPodAutoscaler` under `autoscaling.*`. Every replica must reach
the same Corndogs service.

The head does not autoscale. Adding a storage voter is a placement decision:
enroll the node, then add it as a learner, and the controller promotes it.
Query capacity grows with read replicas, which the `scaled` profile adds.

## 8. Watching the watchers

Alert evaluation, notification delivery, and the projector passes run on
durable queues. `tallyowl_workflow_pending_count` shows each queue's depth,
and the dashboard's workflow table shows lag, failures, and quarantine.
Nothing retries when the sweep stops. A collector that is ready has a running
sweep. A head is not ready until it reaches the durable queue. If the head's
sweep fails later, the head stays ready and `workflow-queue` is degraded.
