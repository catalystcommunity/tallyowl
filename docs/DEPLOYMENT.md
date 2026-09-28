# Deployment and Helm charts

This document uses these abbreviations: certificate authority (CA) and mutual
Transport Layer Security (mTLS). [DOCUMENTATION.md](DOCUMENTATION.md) holds the
full list.

## 1. Purpose

Two Helm charts install TallyOwl. The same charts serve a home lab and a
multi-region installation.

A multi-region installation is several releases of the same charts in several
clusters, and those releases must coordinate. That case is not an afterthought
in this design. Section 5 gives it first-class treatment, because a chart that
only works for one cluster is not useful to the people who need cells.


### One installation, one device

`storage.reserveBytes` holds space back so that recovery can still write when a
device fills. **The reserve belongs to the device and one process enforces it**,
so two installations sharing a device each hold back the same bytes and each
treats them as its own. Both can spend the reserve at once.

No data is at risk from this: a write is still refused when the device cannot
take it, because that check reads the real free space. What is at risk is the
recovery the reserve exists for.

An installation reports its device at start-up and in
`tallyowl_storage_device_info`. Two installations reporting the same number are
sharing a reserve. Give each one a device, or accept that the reserve is
advisory. See FAILURE_MODES.md section 10.

## 2. Charts

| Chart | Installs |
| --- | --- |
| `tallyowl` | Head roles, query, storage, dashboard, controller, and workers |
| `tallyowl-collector` | Intake, forwarder, and compatibility receiver |

A collector holds no durable state. Corndogs owns the queue and the payload, so
a collector stays a stateless Deployment.

Each chart can reference an existing Corndogs service or install a tightly
scoped local one.

A chart never puts a role token, a key, or any other secret in a command line,
a log, or rendered manifest output.

## 3. Profiles

A profile selects placement and replication. It never changes the stored format
or the application integration.

| Profile | Shape |
| --- | --- |
| `home` | One head pod, one volume, one tablet, one voter, `local-one` receipts |
| `home-collector` | One collector, one durable Corndogs copy |
| `home-limits` | No read replica, no cold object store, no separate projector |
| `replicated` | One cell, three controllers, three tablet voters, stateless gateways, separate workers |
| `scaled` | Several cells, a global directory, read and export replicas, role-specific pools |

`home` is the default.

A home installation runs three containers: the head, one collector, and one
Corndogs. The head and the collector share that Corndogs and use separate
queues.

A profile change is an online change. It moves data. It does not rewrite it.

## 3a. Home profile on Kubernetes

This procedure installs the home profile. It uses two releases: the head chart
with Corndogs in the head pod, and the collector chart.

Each release attaches its chart to the GitHub release page as a `.tgz` file.
The commands below use version 0.2.1. The release job also adds each chart to
the `catalystcommunity/charts` repository. This document does not give a Helm
repository address for that repository, because no file in this repository
records one.

This procedure ran in a KinD cluster on 2026-09-26, with the charts from this
repository and a locally built image, as IMPLEMENTATION_LOG.md L192 records.
KinD has no Gateway API, so that run used `dashboard.allowPlaintext=true` in
step 2. No CI job runs the procedure yet.

1. Make the authority and the Secrets for transport security. Section 7c
   explains each file. Keep `root.key` off every TallyOwl host.

   ```sh
   tallyowl-head ca create ./tallyowl-authority
   kubectl create namespace tallyowl
   kubectl --namespace tallyowl create secret tls tallyowl-signing \
     --cert=./tallyowl-authority/intermediate.crt \
     --key=./tallyowl-authority/intermediate.key
   kubectl --namespace tallyowl create secret generic tallyowl-authority \
     --from-file=ca.crt=./tallyowl-authority/root.crt
   ```

   Make the certificate that Corndogs serves. Sign it with the intermediate.
   Its names are the Service that the head chart makes, `tallyowl-corndogs`,
   in both forms:

   ```sh
   openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
     -keyout corndogs.key -out corndogs.csr -subj "/CN=tallyowl-corndogs" \
     -addext "subjectAltName=DNS:tallyowl-corndogs,DNS:tallyowl-corndogs.tallyowl.svc"
   openssl x509 -req -in corndogs.csr -days 90 -copy_extensions copy \
     -CA ./tallyowl-authority/intermediate.crt \
     -CAkey ./tallyowl-authority/intermediate.key -out corndogs.crt
   cat ./tallyowl-authority/intermediate.crt >> corndogs.crt
   kubectl --namespace tallyowl create secret tls tallyowl-corndogs-tls \
     --cert=corndogs.crt --key=corndogs.key
   rm corndogs.key corndogs.csr
   ```

   The `-copy_extensions` option needs OpenSSL 3.0 or later. `intermediate.crt`
   holds the intermediate and the root, so the chain is complete.

   Make a certificate for the collector from your own authority, for example
   with cert-manager. Applications check it, so its names must include the
   address that applications dial, for example
   `tallyowl-collector.tallyowl.svc`. Store it as a Secret of type
   `kubernetes.io/tls` with the name `tallyowl-collector-tls`.

2. Install the head with Corndogs beside it. Corndogs serves TLS with the
   certificate from step 1. Turn on the NetworkPolicy as well: Corndogs does
   not check who calls it (section 7c).

   ```sh
   helm install tallyowl \
     https://github.com/catalystcommunity/tallyowl/releases/download/v0.2.1/tallyowl-0.2.1.tgz \
     --namespace tallyowl \
     --set corndogsDeployment.enabled=true \
     --set corndogsDeployment.tlsSecret=tallyowl-corndogs-tls \
     --set deployment.networkPolicy.enabled=true \
     --set deployment.tls.signingSecret=tallyowl-signing \
     --set 'deployment.tls.authorities[0].secretName=tallyowl-authority' \
     --set gateway.enabled=true \
     --set gateway.parentRef.name=<your Gateway>
   ```

   The dashboard serves plaintext. The chart accepts it only behind a Gateway
   that ends TLS, as above, or with `dashboard.allowPlaintext=true`, or with
   `dashboard.enabled=false`.

3. Make a project and its first key. Section 9 explains why this is a Job and
   why the head stops while the Job runs.

   ```sh
   helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
     --set deployment.maintenance.enabled=true \
     --set 'deployment.maintenance.args={provision,shop}'
   kubectl --namespace tallyowl wait --for=condition=complete --timeout=5m \
     job --selector tallyowl.io/component=maintenance
   kubectl --namespace tallyowl logs --selector tallyowl.io/component=maintenance
   ```

4. Store the key in a Secret. The log shows the key one time only.

   ```sh
   kubectl --namespace tallyowl create secret generic tallyowl-key \
     --from-literal=api-key='<the key from the log>'
   ```

5. Make an operator session for the dashboard. Do this when
   `linkkeys.enabled` is false, which is the default. Keep the token that the
   log shows.

   ```sh
   helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
     --set deployment.maintenance.enabled=true \
     --set 'deployment.maintenance.args={session,create,<your name>}'
   kubectl --namespace tallyowl wait --for=condition=complete --timeout=5m \
     job --selector tallyowl.io/component=maintenance
   kubectl --namespace tallyowl logs --selector tallyowl.io/component=maintenance
   ```

6. Turn the Job off. This deletes the Job and starts the head again.

   ```sh
   helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
     --set deployment.maintenance.enabled=false
   ```

7. Make a role token for the collectors. A collector enrolls with it at each
   start. Run `token create` in the maintenance Job, in the same way as
   steps 3 and 5. The workspace, `default` here, limits what the collectors
   can write. Section 7c explains each word.

   ```sh
   helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
     --set deployment.maintenance.enabled=true \
     --set 'deployment.maintenance.args={token,create,collectors,collector-intake\,collector-forwarder,default}'
   kubectl --namespace tallyowl wait --for=condition=complete --timeout=5m \
     job --selector tallyowl.io/component=maintenance
   kubectl --namespace tallyowl logs --selector tallyowl.io/component=maintenance
   ```

   The log shows the token one time. Store it in a Secret, and then turn the
   Job off as in step 6:

   ```sh
   kubectl --namespace tallyowl create secret generic tallyowl-collector-token \
     --from-literal=token='<the token>'
   helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
     --set deployment.maintenance.enabled=false
   ```

   In `--set`, a comma separates list items, so the comma between the two
   roles has a backslash before it.

8. Install the collector.

   ```sh
   helm install tallyowl-collector \
     https://github.com/catalystcommunity/tallyowl/releases/download/v0.2.1/tallyowl-collector-0.2.1.tgz \
     --namespace tallyowl \
     --set corndogs.endpoint=tallyowl-corndogs:5080 \
     --set head.endpoint=tallyowl:5110 \
     --set 'deployment.tls.certificateSecrets[0]=tallyowl-collector-tls' \
     --set 'deployment.tls.authorities[0].secretName=tallyowl-authority' \
     --set deployment.tls.roleTokenSecret.name=tallyowl-collector-token \
     --set deployment.apiKeySecret.name=tallyowl-key \
     --set deployment.apiKeySecret.key=api-key
   ```

   | Value | Why |
   | --- | --- |
   | `corndogs.endpoint` | The queue Service of the head release. The default is loopback, and the chart refuses it: a collector stops at start when it cannot reach the queue |
   | `head.endpoint` | The head Service. The chart refuses the loopback default |
   | `deployment.tls.certificateSecrets` | The certificate that applications verify the collector against. The chart refuses a render with none |
   | `deployment.tls.authorities` | The authority that the collector verifies the head against |
   | `deployment.tls.roleTokenSecret.name` | The role token the collector enrolls with. The chart refuses a render with no token |
   | `deployment.apiKeySecret.name` | The Secret that holds the key. The chart sets `collector.apiKey` to `env:SECRET_TALLYOWL_API_KEY` and fills that variable from the Secret |
   | `deployment.apiKeySecret.key` | The name of the entry in that Secret; `api-key` by default |

   A native application presents its own key. The collector needs a key only
   for the compatibility receivers and for self-observation. The chart refuses
   those features when the collector has no key.

9. Point an application at `tallyowl-collector.tallyowl.svc:5100`. The app
   driver uses TLS for that address and checks the certificate against the
   authorities of its host. If you made the collector certificate from a
   private authority, give the driver that authority. See
   RUNBOOK_INTEGRATION.md section 2.

The key Secret has the same form in the head chart. The head uses it only when
`metrics.selfObservation.enabled` is true. Then set
`deployment.selfObservationCollector` to the collector Service, for example
`tallyowl-collector:5100`. The head dials `collector.listen` to send its own
metrics, and in a pod that setting is a bind address. The chart refuses
self-observation with no collector address.

## 4. Values that matter

### Installation identity

| Value | Meaning |
| --- | --- |
| `installation.id` | Identifies one logical TallyOwl installation |
| `installation.authorities` | The authorities that every node trusts, as PEM files. The charts fill it from `deployment.tls.authorities` |
| `installation.signingCertificate`, `installation.signingKey` | The intermediate authority that a head signs node certificates with. The head chart fills both from `deployment.tls.signingSecret` |
| `enrollment.certificateLifetimeHours` | The most hours a node certificate is valid. The default is 24 |

Section 7c gives the procedures. The earlier design names
`installation.caBundle` and `installation.caFingerprint`. The loader has
neither, and `installation.authorities` does their work.

### Cell identity

| Value | Meaning |
| --- | --- |
| `cell.id` | Identifies this cell inside the installation |
| `cell.region` | The region label for placement and for write ownership |
| `cell.controllers` | Three or five; the controller quorum size |
| `globalDirectory.endpoints` | **Not built in this release.** The global directory, when one exists |

### Storage and durability

| Value | Meaning |
| --- | --- |
| `storage.receiptPolicy` | `local-one`, `local-quorum`, `remote-one`, or custom |
| `storage.tabletVoters` | Voting replicas for each tablet |
| `storage.coldTier.enabled` | **No effect in this release.** Object storage for cold segments |
| `corndogs.durableCopies` | Copies required before a collector acknowledgement |
| `corndogs.backend` | `file` or `postgres` |
| `corndogs.maxPayloadBytes` | Corndogs payload limit; 16 MiB by default |
| `corndogs.maxDeliveryAge` | How long a batch may keep being retried; 24 hours by default |
| `storage.deduplicationWindow` | How long the head remembers a batch ID; 72 hours by default |

### Replication and placement

A home installation leaves `replication.listen` empty and none of the rest
applies. An operator turns replication on by giving the node an address that
peers can reach: no address, no listener, no cluster.

| Value | Meaning |
| --- | --- |
| `replication.listen` | Where this node answers other storage nodes. Empty in the home profile |
| `replication.advertise` | The address other nodes dial for this node. The head chart sets it for each pod. See "Node names and addresses in a cluster" below |
| `replication.peers` | The peers a first bootstrap starts with. A cell learns its peers from its controller quorum afterwards |
| `replication.writeTimeout` | How long a write waits for its tablet group to commit; 10 seconds by default |
| `node.name` | What the control plane calls this node. Empty means the head assigns one at enrollment |
| `node.failureDomain` | The rack, zone, or host this node is in |
| `placement.mode` | `automatic`, `recommendation-only`, or `paused`; recommendation-only by default |
| `placement.splitAbove` | Split a tablet larger than this; 64 GiB by default |
| `placement.mergeBelow` | Merge two adjacent tablets smaller than this; 8 GiB by default |
| `placement.concurrentChanges` | Splits, merges, and movements that may run at once in one cell; one by default |
| `query.maxFanOut` | Tablets one query may ask at once; 256 by default |

`replication.listen` must have an address when `storage.tabletVoters` is
greater than one, and TallyOwl refuses to start when it does not. A node with
peers and no address to be reached at elects nothing and refuses every write,
which reads as a storage fault rather than as a missing setting.

#### Node names and addresses in a cluster

A consensus ID is a hash of a node name. A node names each peer from the peer
address: `node-`, then the address with each `.` and each `:` replaced by `-`.
A node and its peers must get the same name for that node, or each node holds
a different voter set and no group forms.

The head chart therefore does these things when `replication.listen` has a
value:

- It gives each pod the address
  `<pod>.<release>-nodes.<namespace>.svc:<port>` in the environment variable
  `TALLYOWL_REPLICATION__ADVERTISE`. This is the `replication.advertise`
  setting.
- It sets `node.name` for each pod to the name that the rule above gives for
  that address.
- It writes `replication.peers` as one such address for each replica, when the
  value is empty. Each node removes its own address from the list.
- It refuses a peer address that is a bind address or a loopback address.
  `replication.listen` is what the process binds. In a pod that is `0.0.0.0`,
  and no peer can dial it.
- The headless Service publishes addresses that are not ready, because a group
  forms before a pod is ready.

`helm-check` renders a cell of three and proves each of these from the rendered
output.

`replication.advertise` needs the head to read it. A head that does not know
the setting ignores it and advertises its bind address, and a replicated
installation from the chart then cannot form a group. Check the release notes
of your release for `replication.advertise` before you install more than one
replica.

`placement.mergeBelow` must be below half of `placement.splitAbove`, and
TallyOwl refuses to start when it is not. Two merged tablets that were
immediately over the split threshold would make a cell rewrite the same data for
ever. See CELLS.md section 6 on hysteresis.

`placement.mode` is `recommendation-only` by default. The controller decides in
every mode and the mode selects whether it acts, so the automatic path is
exercised at every installation rather than being code nobody ran until the day
it was switched on.

`corndogs.maxPayloadBytes` must exceed the batch seal size in D19, which is
512 KiB.

`storage.deduplicationWindow` must exceed `corndogs.maxDeliveryAge`, and
TallyOwl refuses to start when it does not. D36 makes the two one decision: a
retry that arrives after the head has forgotten the batch ID commits a second
logical batch, and no query can remove it afterwards.

### Dashboard

| Value | Meaning |
| --- | --- |
| `dashboard.enabled` | Serve the dashboard; on by default |
| `dashboard.listen` | Where the dashboard serves its document, its bundle, and the browser carrier |
| `dashboard.assets` | Where the built dashboard bundle is |
| `dashboard.callbackPath` | The path a LinkKeys sign-in returns to |

`dashboard.listen` is the origin `linkkeys.callbackUrl` must name, and
`dashboard.callbackPath` must be that URL's path. A sign-in returns to an
address nothing serves when the two disagree.

The dashboard carries `TallyOwlControl` and refuses every other service, so it
is not an ingest surface. Telemetry reaches a collector over CSIL.

### Integrity and recovery

| Value | Meaning | Default |
| --- | --- | --- |
| `integrity.mode` | `none`, `verify-on-read`, or `scrub`. **No effect in this release** | `verify-on-read` |
| `integrity.scrub.period` | How long one full pass takes. **No effect in this release** | 7 days |
| `integrity.scrub.rateLimit` | Read bandwidth the scrub may use. **No effect in this release** | 16 MiB each second |
| `catalog.snapshots.enabled` | Periodic catalog snapshots. **No effect in this release** | `false` |
| `catalog.snapshots.period` | Time between snapshots. **No effect in this release** | 1 hour |
| `catalog.snapshots.keep` | Snapshots retained. **No effect in this release** | 2 |
| `placement.slowNode.action` | `alert` or `demote` | `alert` |
| `placement.slowNode.factor` | Multiple of the group median that counts as slow. **No effect in this release** | 4 |
| `placement.slowNode.duration` | How long it must hold before the state changes. **No effect in this release** | 5 minutes |
| `compaction.gcGrace` | Wait before deleting an unpinned generation | 1 hour |
| `storage.reserveBytes` | Space held back so recovery can still write | 1 GiB |

#### Settings that have no effect in this release

The loader reads each setting below and no code uses the value. The setting
stays in the configuration so that a later release can use it with no change
to a values file. Do not rely on any of them.

| Setting | What the release does |
| --- | --- |
| `integrity.mode` | The store verifies on each read, for each value. `none` does not turn the check off, although the head logs that it is off. `scrub` starts no scrub, and nothing repairs a damaged segment in the background |
| `integrity.scrub.period`, `integrity.scrub.rateLimit` | No scrub runs |
| `catalog.snapshots.enabled`, `.period`, `.keep` | No periodic catalog snapshot is made. The recovery for a lost catalog is `tallyowl-head rebuild` or a restore. See section 9 |
| `storage.coldTier.enabled` | No cold tier is connected. All segments stay on the local volume |
| `placement.slowNode.factor`, `placement.slowNode.duration` | No code measures a slow node. `placement.slowNode.action` is read and nothing calls the decision that uses it |
| `retention.audit` | No pass expires audit records |

D57 gives the design for `integrity.mode`. When it is built, `none` turns off
every integrity check, and the dashboard shows that state, because an operator
who inherits an installation must not have to discover it.

`compaction.gcGrace` must exceed the maximum query runtime the budget permits.
The chart refuses a value that does not. See
[FAILURE_MODES.md](FAILURE_MODES.md) section 8.1.

The chart refuses `storage.receiptPolicy: local-one` when
`storage.tabletVoters` is greater than one. `local-one` is legal only for a
single-voter tablet. See D27.

The chart refuses `corndogs.durableCopies` greater than one on a backend that
cannot satisfy it. See D4.

## 5. Multi-cluster and multi-region

**This section is a design. This release does not build it.** The loader has
no `installation.caBundle`, `installation.caFingerprint`,
`globalDirectory.endpoints`, or `globalDirectory.bootstrap` setting, and the
charts have no such value. The project directory is held in memory on each
node. Helm cannot reach a directory at render, so the chart makes none of the
refusals that this section names. A multi-region installation is not possible
with this release.

A large installation is several chart releases that must agree about identity
and disagree about location. Getting that split wrong is the most common way to
break a distributed installation.

### Values that must match everywhere

Every release in one installation uses identical values for these:

- `installation.id`;
- `installation.caBundle` and `installation.caFingerprint`;
- the segment format version range that the release supports;
- the protocol version range that the release supports;
- `globalDirectory.endpoints`, once a global directory exists.

A mismatch in any of these produces a node that enrolls and then cannot join.
The chart fails the installation rather than starting a node that will not
work.

### Values that must differ

Each cell release sets its own:

- `cell.id`, unique in the installation;
- `cell.region`;
- storage volume classes and sizes appropriate to that cluster;
- collector endpoints local to that cluster.

The chart refuses a duplicate `cell.id` when it can reach the global directory.

### Bootstrap order

1. **Not built.** Install the first cell with `globalDirectory.bootstrap:
   true`. That release creates the installation ID.
2. Give each cell the same root authority in `deployment.tls.authorities`, and
   an intermediate that the root signed in `deployment.tls.signingSecret`. Make
   a role token for the collectors of each cell.
3. Install each additional cell with the same installation identity values and
   its own cell identity values.
4. Each new cell enrolls its nodes against the existing CA.
5. Register the new cell with the global directory.
6. Assign or move projects to the new cell.

Step 2 is the only manual handoff. A role token is a reusable credential with
its own expiry, use count, and network restrictions. See
[NODE_IDENTITY.md](NODE_IDENTITY.md).

### Cross-cluster connectivity

- a collector needs to reach its head over TCP, with mutual TLS. See section
  7c;
- a storage node needs to reach the other voters of its tablets;
- a cell controller needs to reach the global directory, but a cell continues
  data operations without it;
- a collector never needs to reach another cluster's collector.

A storage node does not need to share a Kubernetes cluster with a collector.

### Write ownership

Each tablet has one write region at one time. A fenced control operation
changes it. Different tablets can have different write regions, so an
installation writes in many regions without a multi-writer conflict for one
tablet. See D27.

The chart does not decide write ownership. The controller does.

## 5a. Infrastructure requirements

TallyOwl states what it needs from infrastructure. It does not restate another
product's durability numbers as its own objective. See D53.

| Requirement | Why |
| --- | --- |
| A volume that honours `fsync` | Every durability claim rests on it. A volume that acknowledges a flush without one makes every receipt a lie. |
| A storage class with the redundancy the operator needs | TallyOwl acknowledges at the configured receipt policy. Surviving the loss of the volume itself is the infrastructure's job. |
| An object store with its own durability guarantee | The cold tier holds retained telemetry. Its durability is the bucket's. |
| A backup target that the operator tests | A restore that nobody has run is not a backup. |

A process crash needs no recovery procedure. Writes are atomic, and a restart
loses nothing that TallyOwl acknowledged.

A region loss is a convergence problem rather than a recovery one. A surviving
region continues, and CELLS.md gives the fenced failover and the watermarks
that show convergence.

## 6. Placement and disruption

- spread tablet voters across failure domains with anti-affinity rules;
- a disruption budget preserves intake capacity and query capacity;
- a disruption budget never overrides safe draining;
- a storage pod keeps its node ID on its persistent volume, so a replacement
  pod requests a certificate for that same node ID;
- a stateless pod gets a new node ID for each start.

The head chart carries three groups of values for this. A home installation has
one replica and none of them applies to it.

| Value | Meaning |
| --- | --- |
| `topologySpread.constraints` | Spread replicas across zones and then across nodes, with a skew of one |
| `affinity.antiAffinity` | `required` by default: two voters of one tablet never share a node |
| `disruption.minAvailable` | How many replicas must stay during a voluntary disruption. Empty by default, which means a majority: two of three, three of five |

`disruption.minAvailable` is a count and not a percentage. A percentage of a
shrinking set rounds in the wrong direction, and this number has to hold when
the set is already smaller than it should be.

The chart calculates a majority as floor(replicas / 2) + 1 and refuses a lower
value. A budget of two with five voters lets one drain remove three voters, and
that ends the quorum.

The collector chart has its own values. A collector holds no durable state, so
its rules are weaker.

| Value | Meaning |
| --- | --- |
| `topologySpread.constraints` | Spread collectors across nodes with `ScheduleAnyway`. A collector that cannot spread is better than no collector |
| `disruption.maxUnavailable` | One by default. The budget applies when there is more than one replica or when autoscaling is on |

### Resources

Each chart sets a CPU request, a memory request, and a memory limit. A pod with
no request has the BestEffort class, and a node evicts that class first.

| Pod | Request | Memory limit | Why |
| --- | --- | --- | --- |
| Head | 1 CPU, 4 GiB | 12 GiB | BENCHMARKS.md section 24 measured 2.1 GiB resident with the locator cache bounded, and 7.5 GiB before that. One consolidation pass holds its rows decompressed, and the limit is sized for the 32 MiB default of `compaction.coldGroupBatch`. Raise the limit before you raise that setting |
| Corndogs beside the head | 0.25 CPU, 256 MiB | 1 GiB | The queue holds its index in memory and its payloads on the volume |
| Collector | 0.5 CPU, 256 MiB | 1 GiB | A collector holds one batch for each request in flight and keeps no data |

There is no CPU limit. A CPU limit slows a seal and a consensus heartbeat
together, and a late heartbeat starts an election.

The collector chart refuses `autoscaling.enabled` with no
`resources.requests.cpu`. The autoscaler measures CPU against the request, so
with no request it never scales.

### Pod security

Each pod meets the Pod Security `restricted` profile: a non-root user, the
`RuntimeDefault` seccomp profile, no privilege escalation, and no capabilities.
The head and the collector have a read-only root file system. The head writes
only below `head.dataDir`, which is on the data volume. With
`persistence.enabled: false` that volume is an `emptyDir`.

The Corndogs image has a version tag that matches the queue client in the
workspace `Cargo.toml`. The chart refuses the tag `latest`, because the queue
carries every receipt and its version must not change when a pod moves. The
chart also refuses Corndogs beside the head with `persistence.enabled: false`.
A collector would call a batch durable that a pod restart deletes.

A node reports its own domain in `node.failureDomain`, and the controller places
at most one voter of a tablet in each domain. When a cell has fewer domains than
a tablet has voters, the controller still places the tablet and reports that one
domain's loss can end its quorum. It does not refuse, because refusing to place
data is worse than placing it and saying what the risk is.

## 7. Upgrade order

1. Upgrade the head ingest and query roles first, because the head accepts the
   current and the previous protocol version.
2. Upgrade storage nodes one failure domain at a time.
3. Upgrade collectors last, so no collector speaks a version the head does not
   accept.
4. Upgrade cells one at a time in a multi-region installation.

The compatibility window opened at the first release, 0.2.0, and it is
enforced rather than promised. An app driver declares the protocol version it
speaks on every batch, and a collector declares its own when it forwards.
Collector intake and the head each accept the current version and the one
before it, from one list. A version outside the window is refused with a
message that names both ends, and `tallyowl_protocol_version_refused_total`
counts it, by version.

That is what makes the order above safe: the head is upgraded first, so every
collector still running is one version behind the head it forwards to, which is
inside the window by construction. Support for a protocol version ends one
minor release after the release that replaces it, and the release notes say so
before it ends. There is one protocol version today, so nothing a current
client sends is refused. See D31, `docs/RELEASE_NOTES.md`, and L183.

### What a pod does when it stops

The kubelet sends SIGTERM, waits for
`deployment.terminationGracePeriodSeconds`, and then sends SIGKILL. The default
is 120 seconds for the head and 60 seconds for a collector.

The paragraph below is true **when the SIGTERM handler is present in your
release**. Releases up to 0.2.1 have no handler. In those releases a process
ignores SIGTERM in a container, because it is process 1 and a signal with no
handler does not stop process 1. The pod then runs until the grace period ends
and the kubelet sends SIGKILL. Readiness does not fail first, and a claimed
task waits for the timeout sweep. No acknowledged data is lost, because
delivery is at-least-once. Each restart costs the grace period and one claim
timeout. With such a release, set `deployment.terminationGracePeriodSeconds`
to a small value, for example 5, so that a rollout does not wait two minutes
for each pod.

With the handler, a head fails readiness, stops its listeners, seals its open
rows, and exits with status 0. A collector fails readiness, stops intake,
finishes or releases its claimed tasks, and exits with status 0. Queued data
stays durable in both cases.

The image needs no init process. A process that installs a handler receives
SIGTERM as process 1. The kernel drops only a signal that has no handler.

## 7b. Reaching an installation

### What a pod binds

The settings tree keeps the loader's own defaults, and those are loopback: a
process on a workstation must not open a port to the network because somebody
started it. A pod is the other case. A loopback bind there reaches nothing, and
the Service in front of it forwards to a port no client can use.

Each chart therefore rewrites the host part of each address that its service
listens on, from the deployment value `bindAddress`, which defaults to
`0.0.0.0`. The head chart rewrites `head.listen`, `head.operationalListen`,
`dashboard.listen`, and `replication.listen`. The collector chart rewrites
`collector.listen`, `collector.operationalListen`, and the OpenTelemetry
listener. The other addresses keep their loopback defaults, because a service
does not bind them, and a network address there makes `config check` ask for
certificates that the service does not use. The port always comes from the setting, so a changed port reaches
the process and the Service together. `replication.listen` stays empty when it
is empty, because an empty value means a node with no replication port.

Set `bindAddress` to `127.0.0.1` for a pod whose only client is a sidecar.

### The dashboard bundle

The container image carries the dashboard at
`/usr/local/share/tallyowl/dashboard`, and the deployment value
`dashboardAssets` points the head at it. The setting keeps the loader default,
which is the path a developer builds into. An image without the bundle serves
an empty page, which is why `helm-check` refuses a rendered configuration whose
`dashboard.assets` is a relative path.

### Gateway API

Traffic from outside the cluster reaches the dashboard through an **HTTPRoute**.
The charts render one when `gateway.enabled` is true, and refuse to render a
route with no `gateway.parentRef.name`, a dashboard route with the dashboard
turned off, or a dashboard route on the collector chart, which serves none.

```sh
helm install tallyowl \
  https://github.com/catalystcommunity/tallyowl/releases/download/v0.2.1/tallyowl-0.2.1.tgz \
  --set gateway.enabled=true \
  --set gateway.parentRef.name=platform \
  --set gateway.hostnames[0]=tally.example.com
```

**No Gateway is created.** A Gateway carries a listener and an address that
belong to the platform, and a chart that created one would be claiming both.

**Ingest gets no route.** Native TallyOwl traffic is CSIL over TCP and an
application reaches the collector directly. An HTTPRoute in front of intake
would be a generic HTTP ingest API, which `AGENTS.md` forbids.

**There is no Ingress template.** Gateway API is the interface these charts
support. Ingress can be added when somebody needs it.

## 7c. Transport security

D62 gives the rule. A CSIL connection that crosses a network uses TLS. In a
pod, every listener is on the pod network, so every CSIL connection uses TLS.

| Connection | Transport | The server shows | The client shows |
| --- | --- | --- | --- |
| Application to collector intake (`collector.listen`) | TLS | A certificate from `deployment.tls.certificateSecrets` | Its project key |
| Exporter to the OpenTelemetry receiver | TLS | The same certificate | Nothing. See THREAT_MODEL.md boundary 8 |
| Collector to head (`head.listen`) | Mutual TLS | The head node certificate, for the name `head.tallyowl.internal` | Its node certificate |
| Head to head (`replication.listen`) | Mutual TLS | A node certificate | A node certificate |
| Collector to Corndogs (`corndogs.endpoint`) | TLS | A certificate from `corndogsDeployment.tlsSecret`, signed by the installation authority | Nothing |
| Head to its Corndogs sidecar | TLS, on loopback | The same certificate, for the name `<release>-corndogs.<namespace>.svc` | Nothing |
| Operational endpoint, dashboard | **Plaintext** | Nothing | Nothing |

### Corndogs

Corndogs serves TLS from release 0.7.6 (image `0.7.6`, chart 0.5.7). There are
two ways to run it:

- **The sidecar in the head pod.** Use it for the home profile, with one head.
  The steps are below.
- **A shared Corndogs from the Corndogs chart.** Use it when the head release
  has more than one replica, because each head then needs the same queue. The
  steps are under "A shared Corndogs".

The head chart gives the Corndogs sidecar a certificate from
`corndogsDeployment.tlsSecret`. It refuses a render that exposes the sidecar
to collectors with no certificate, unless `transport.allowPlaintext` is set.

1. Make a certificate with the DNS names `<release>-corndogs` and
   `<release>-corndogs.<namespace>.svc`. Sign it with the installation
   authority.
2. Store it in a `kubernetes.io/tls` Secret, and name the Secret in
   `corndogsDeployment.tlsSecret`.
   The sidecar pulls its image by `corndogsDeployment.imagePullPolicy`. When
   that is empty, it uses `image.pullPolicy`. Set it when the two images come
   from different places, for example a TallyOwl image built locally with
   `image.pullPolicy: Never`.
3. Install the head chart. The head reaches its sidecar over TLS, by the
   second name, because Corndogs serves TLS on its one RPC port.
4. The collector chart trusts the first entry of
   `deployment.tls.authorities` for Corndogs. It checks the host of
   `corndogs.endpoint`, which is the first name.

#### A shared Corndogs

The Corndogs chart is not in a chart registry. Its GitHub release attaches
the packaged chart.

1. Make a certificate with the DNS names `corndogs` and
   `corndogs.<namespace>.svc`. Sign it with the installation authority. Store
   it in a Secret of type `kubernetes.io/tls` that also holds the root as
   `ca.crt`:

   ```sh
   kubectl --namespace <namespace> create secret generic corndogs-tls \
     --type=kubernetes.io/tls --from-file=tls.crt=corndogs.crt \
     --from-file=tls.key=corndogs.key --from-file=ca.crt=root.crt
   ```

2. Install Corndogs. The file backend keeps one replica and needs no
   database:

   ```sh
   helm install corndogs \
     https://github.com/catalystcommunity/corndogs/releases/download/helm_chart%2Fv0.5.7/corndogs-0.5.7.tgz \
     --namespace <namespace> \
     --set storage.backend=file --set postgresql.enabled=false \
     --set zalando_postgres.enabled=false \
     --set tls.enabled=true --set tls.secretName=corndogs-tls --set tls.caKey=ca.crt
   ```

   The chart's `ca.crt` entry is for its own timeout CronJob, which checks the
   server with it.

3. Install the head chart with `corndogsDeployment.enabled=false` and
   `corndogs.endpoint=corndogs.<namespace>.svc:5080`. The head trusts the
   first entry of `deployment.tls.authorities` for Corndogs, and checks the
   host of the endpoint. Give the collector chart the same endpoint.

Corndogs reads its certificate files again after a change, so a renewed Secret
needs no restart.

A service that is not in a chart sets `corndogs.tls.caFile` (a PEM file of
the authorities) and, if the certificate does not carry the endpoint's host,
`corndogs.tls.serverName`. An empty `caFile` means the trusted authorities of
the operating system. A loopback endpoint with no `caFile` stays plaintext.

### The two exceptions

- **The operational endpoint.** Probes and scrapers inside the cluster read it.
  It carries no secret, and a metric carries no personal data.
- **The dashboard.** It carries session tokens. The head chart accepts it only
  behind a Gateway that ends TLS (`gateway.enabled` and
  `gateway.dashboard.enabled`), or with `dashboard.allowPlaintext=true`.

### What the charts refuse

The charts refuse a render with no transport security. The refusal names the
value to set.

- The head chart needs `deployment.tls.signingSecret` and
  `deployment.tls.authorities`.
- The collector chart needs `deployment.tls.certificateSecrets`,
  `deployment.tls.authorities`, and `deployment.tls.roleTokenSecret.name`.
- Each chart accepts `transport.allowPlaintext=true` in place of these. Set it
  only when something else protects the network, for example a service mesh
  with mutual TLS. Each service then writes a warning at start that names the
  exposed listeners.

`./tools.sh helm-check` renders each profile and runs the `config check` of the
service on the configuration that the chart renders.

### First installation

1. Make a root authority and an intermediate authority:

   ```sh
   tallyowl-head ca create ./tallyowl-authority
   ```

   The command writes four files. It refuses to replace a file that exists.

   | File | Where it goes |
   | --- | --- |
   | `root.crt` | Every head and every collector: `installation.authorities` |
   | `root.key` | **Nowhere.** Keep it offline. It signs the next intermediate |
   | `intermediate.crt` | Every head: `installation.signingCertificate` |
   | `intermediate.key` | Every head: `installation.signingKey`, as a `file:` reference |

2. On Kubernetes, store the files as Secrets. Section 3a gives the commands.
   The charts mount each Secret read-only, with mode 0440.

3. Give the collector a certificate for applications, from your own
   authority. Its names must include each address that applications dial. Put
   the certificate chain in `tls.crt` and the key in `tls.key`, in one
   directory. The collector chart takes a Secret of type `kubernetes.io/tls`.

4. Make a role token for the collectors (below), and give it to each
   collector in `enrollment.roleToken`.

On a host with no Kubernetes, put the files on the host and set the settings in
the first column directly. A key file must be readable only by the service
user.

### Make a role token

A collector enrolls with a role token at each start.

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

### Replace a collector certificate

The collector presents the certificate that is valid now and that started
last. It reads its certificate files again every `tls.reloadInterval` (30
seconds by default).

1. Make the new certificate. Store it in a second Secret.
2. Add the Secret as the second entry of `deployment.tls.certificateSecrets`.
   Upgrade the release. The pods restart one at a time.
3. When the new certificate is valid, the collector presents it. No
   connection stops.
4. Remove the old Secret from the list after the old certificate expires.

On a host, add a second directory to `tls.certificateDirectories`. The
collector reads it with no restart.

`tallyowl_tls_certificate_expiry_seconds` gives the time until the last loaded
certificate expires. `deploy/monitoring/` warns 14 days before.

### Replace the authority

Each service trusts a list of authorities, and reads the files again every
`tls.reloadInterval`.

1. Make a new root and intermediate with `tallyowl-head ca create` in a new
   directory.
2. Add the new root to `installation.authorities` on every head and every
   collector. Keep the old root. Wait until every service has read the list.
3. Change `installation.signingCertificate` and `installation.signingKey` on
   every head to the new intermediate. Restart the heads. New certificates now
   come from the new authority.
4. Wait for one certificate lifetime (`enrollment.certificateLifetimeHours`).
   Every node then holds a certificate from the new authority.
5. Remove the old root from `installation.authorities`.

### Node certificates and a head outage

A node certificate is valid for `enrollment.certificateLifetimeHours` (24 by
default). A node renews it when one third of that time is left. There is no
revocation list: revoke a node or its role token, and the node cannot renew,
so it stops within one lifetime.

If the head is not available for longer than one third of the lifetime (8
hours at the default), a collector certificate expires. Then:

- intake continues. It uses the application certificate from files, and it
  writes each batch to Corndogs;
- delivery to the head stops until the head is available;
- when the head is available, the collector enrolls again with its role token,
  and delivery continues from Corndogs.

Nothing is lost while Corndogs has space. A collector that restarts enrolls
again at start.

## 8. Required tests

1. Chart rendering for every profile, with no cluster.
2. Chart values and the configuration loader agree on every key, every type,
   and every required value for the `home` profile. This test fails when a
   setting reaches only one of them. It is what keeps the local development
   loop matching a deployment. See PLAN.md Phase 1.
3. A refused `local-one` on a multi-voter tablet.
4. A refused `durable_copies` value that the backend cannot satisfy.
5. A refused duplicate `cell.id`. **Not built:** section 5 is a design.
6. A refused installation with a mismatched CA fingerprint. **Not built:**
   section 5 is a design.
7. An install, an upgrade, and a rollback in a disposable cluster.
8. A second cell joining an existing installation.
9. A cell that continues data operations while the global directory is down.
10. A rolling upgrade with live traffic and adjacent protocol versions.
11. A refused `compaction.gcGrace` shorter than the maximum query runtime.
12. A replicated cell whose rendered node names, advertised addresses, and
    peer list agree for each pod.
13. A refused disruption budget below a majority.
14. A refused collector with a loopback queue address or head address.
15. A refused Corndogs beside the head with no volume, and a refused `latest`
    tag.
16. A refused autoscaler with no CPU request.
17. A key that reaches a pod as a Secret reference only.
18. In a cluster: an application's events that cross each hop over TLS and
    that the head commits, and a collector that enrolls with a role token.
19. In a cluster: an application that trusts a different authority is refused
    and counted.
20. In a cluster: a replaced certificate Secret is served with no restart.
21. In a cluster: three heads with a shared Corndogs form their groups, become
    ready, and fail over when the leader stops.

`./tools.sh helm-check` does tests 1, 3, 4, and 11 to 17. `./tools.sh
kind-check` does tests 18 to 21 and the install part of test 7. The upgrade
and the rollback of test 7 are not built yet. See CI-CD.md section 5.

## 9. Administration, backup, restore, and rollback

### Why an administration verb is a Job

Each verb of `tallyowl-head` that changes the store opens the data directory.
These verbs are `provision`, `project`, `key`, `session`, `snapshot`,
`restore`, and `rebuild`. One process owns one data directory, and a running
head holds the lock. `kubectl exec` into the head pod therefore fails with
"Stop the head first". The head is the main process of the container, so
stopping it stops the container.

The head chart runs one verb as a Job that mounts the data volume of one pod.
While `deployment.maintenance.enabled` is true, the StatefulSet renders with
no replicas. The Job tries again while the head pod stops.

**This procedure needs downtime.** Collectors continue to accept telemetry and
the queue holds it. Queries, the dashboard, and alerts stop. The correct fix is
a control operation on the running head. That needs a change to the CSIL
control interface, and the project owner has not approved one.

```sh
# 1. Stop the head and run the verb.
helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
  --set deployment.maintenance.enabled=true \
  --set 'deployment.maintenance.args={key,list}'

# 2. Wait, then read the result.
kubectl --namespace tallyowl wait --for=condition=complete --timeout=5m \
  job --selector tallyowl.io/component=maintenance
kubectl --namespace tallyowl logs --selector tallyowl.io/component=maintenance

# 3. Delete the Job and start the head.
helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
  --set deployment.maintenance.enabled=false
```

To run a second verb before step 3, run step 1 again with other arguments. The
Job name contains a hash of the arguments, so each verb gets a new Job.

A key or a session token appears in the Job log one time. Copy it into a
Secret, then do step 3, which deletes the Job and its log.

`tallyowl-head --help` lists the verbs of your release. The verbs of this
release are:

| Arguments | Result |
| --- | --- |
| `{provision,<project>}` | Makes the project if it is missing, and prints one new key |
| `{project,list}` | Lists the workspaces and projects |
| `{key,list}` | Lists the keys, their project, and their state |
| `{key,revoke,<key-id>}` | Stops a key. A collector continues to accept it for `collector.keyCacheTtl` |
| `{session,create,<name>}` | Signs a person in and prints one session token |
| `{session,list}` | Lists the sessions and their state |
| `{session,revoke,<id>}` | Ends one session |
| `{snapshot,<directory>}` | Copies the installation into the directory |
| `{restore,<directory>}` | Restores a snapshot into an empty data directory |
| `{rebuild}` | Builds the list of stored files again from the files |

In a replicated cell the Job opens the volume of one pod, which
`deployment.maintenance.ordinal` selects. This document does not say how a
control record made on one node reaches the other nodes. Test this in your
cell before you rely on it.

### Backup

A snapshot needs the lock on the data directory, so no CronJob can take one
while the head runs. **There is no backup without downtime in this release.**
The chart has no backup schedule for that reason. Use one of these:

- A volume snapshot from your storage class, taken while the head runs. The
  store recovers from a crash-consistent image of its volume, because each
  write is atomic. Make sure that your storage class takes an atomic snapshot
  of the whole volume.
- The `snapshot` verb as a maintenance Job, with a second volume for the
  result:

  ```sh
  helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
    --set deployment.maintenance.enabled=true \
    --set deployment.maintenance.backupClaim=tallyowl-backup \
    --set 'deployment.maintenance.args={snapshot,/var/lib/tallyowl/backup}'
  ```

  Run it again into the same directory and it copies only the new files.

A snapshot holds the stored files, the catalog, and the erasure records. It
does not hold the queue. Data that a collector acknowledged and the head did
not commit is in Corndogs, below `corndogs` on the same volume.

### Restore

`restore` writes only into an empty data directory, so that a restore never
mixes two installations. Restore verifies each checksum before it publishes
anything. A missing or damaged file stops the restore, and the data directory
does not change.

```sh
# 1. Stop the head and move the old data directory aside.
helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
  --set deployment.maintenance.enabled=true \
  --set 'deployment.maintenance.command={mv,/var/lib/tallyowl/data/head,/var/lib/tallyowl/data/head.before-restore}'
kubectl --namespace tallyowl wait --for=condition=complete --timeout=5m \
  job --selector tallyowl.io/component=maintenance

# 2. Restore the snapshot from the backup volume.
helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
  --set deployment.maintenance.backupClaim=tallyowl-backup \
  --set 'deployment.maintenance.command=null' \
  --set 'deployment.maintenance.args={restore,/var/lib/tallyowl/backup}'
kubectl --namespace tallyowl wait --for=condition=complete --timeout=30m \
  job --selector tallyowl.io/component=maintenance
kubectl --namespace tallyowl logs --selector tallyowl.io/component=maintenance

# 3. Start the head.
helm upgrade tallyowl <chart> --namespace tallyowl --reuse-values \
  --set deployment.maintenance.enabled=false
```

The log of step 2 must contain "Nobody who asked to be removed has come back."
If it does not, do not do step 3. Delete `head.before-restore` only after the
restored installation answers queries correctly.

On a host with no cluster, the commands are the verbs themselves:

```sh
# Stop the head first.
mv ./data/head ./data/head.before-restore
tallyowl-head --config tallyowl.yaml restore /backup/tallyowl
# Start the head.
```

A restore loses all that the head committed after the snapshot, unless the
queue still holds it. Choose the snapshot period together with
`corndogs.maxDeliveryAge`.

### Upgrade and rollback

Upgrade in the order of section 7. `helm upgrade` replaces one pod at a time.

A rollback is `helm rollback <release> <revision>`. It is safe only when the
newer release did not write a format that the older release cannot read.

- A segment carries a format major version and a format minor version. A
  reader accepts a minor version that it does not know and skips the fields it
  does not know. A reader refuses a major version that it does not know. See
  SEGMENT_FORMAT.md sections 2 and 4.
- So after a release with a new major version has written segments, an older
  release cannot read those segments. It refuses each one with a message that
  names both versions, and its answers are incomplete. It does not convert
  them.
- RELEASE_NOTES.md must say when a release changes a stored format. Read it
  before each upgrade.
- For such a release, take a snapshot or a volume snapshot before the upgrade.
  A rollback is then a restore of that snapshot into an empty directory, with
  the older release. All data that the newer release committed is lost, unless
  the queue still holds it.
- The protocol window makes a collector rollback safe. The head accepts the
  current protocol version and the one before it.

No test has done an upgrade and a rollback in a cluster. Test 7 of section 8
is that test.

## 10. Monitoring TallyOwl itself

`deploy/monitoring/` holds alert rules for TallyOwl itself and a short
procedure. Each chart can add scrape annotations with
`deployment.podAnnotations`, or a ServiceMonitor with
`deployment.serviceMonitor.enabled` in a cluster that has the Prometheus
operator custom resources. [ALERTS.md](ALERTS.md) is about alerts on your own
telemetry, not about alerts on TallyOwl.
