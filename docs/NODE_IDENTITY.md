# Node identity and automated enrollment

## 1. Credential types

TallyOwl uses three credential types:

- A source key authorizes telemetry for a project.
- A role token authorizes automation and node enrollment.
- A node certificate identifies one enrolled process.

These credentials have different scopes. Do not use one credential as a
replacement for another credential.

## 2. Role token

A role token is a high-entropy bearer credential. An operator or automation
system can use it many times when its policy permits reuse.

TallyOwl stores a token ID and a keyed digest. It does not store the token
value.

A role-token policy can contain:

- permitted node roles;
- permitted cells and regions;
- permitted projects or workspaces;
- an expiration time;
- an optional maximum use count;
- an optional active-node limit;
- optional network restrictions;
- permitted certificate lifetime;
- request rate limits;
- audit labels.

An installation can have many active role tokens. This permits token rotation
without a coordinated stop.

Token revocation prevents new enrollment. It does not immediately revoke
certificates by default. An operator can select cascade revocation.

## 3. Permitted roles

A role token can permit these roles:

- collector intake;
- collector forwarder;
- compatibility receiver;
- ingest gateway;
- query coordinator;
- projector;
- workflow worker;
- read replica;
- export replica;
- storage process.

A role token can enroll a storage process. It cannot assign a tablet to that
process. The cell controller controls placement.

A role token cannot create a controller voter. It cannot create a global
directory voter. It cannot change a tablet voter set.

An administrator starts each voter change. The applicable controller quorum
commits the change.

## 4. Enrollment sequence

Use this sequence to enroll a node:

1. Generate a private key on the node.
2. Read the configured CA or controller fingerprint.
3. Make a TLS connection to the controller.
4. Verify the controller identity.
5. Send the role token and a certificate request.
6. Send the requested role, cell, region, and capabilities.
7. Receive the assigned node ID and certificate chain.
8. Close the bootstrap connection.
9. Make a new mutual TLS connection.
10. Send the node hello message.

### 4.1 What this release builds

D62 gives the transport rule. This release builds steps 1 to 9. Step 10 and
section 5 are not built.

- **The authority.** The operator supplies an intermediate authority to each
  head (`installation.signingCertificate` and `installation.signingKey`).
  `tallyowl-head ca create` makes a root and an intermediate. The catalog holds
  no authority key. A head refuses a signing certificate that is not an
  authority, that expired, or that does not chain to one of
  `installation.authorities`.
- **A head.** A head issues its own node certificate with the intermediate. It
  needs no role token. Its role is `storage-process`, and its certificate also
  carries the name `head.tallyowl.internal`.
- **A collector.** A collector makes a new private key in memory at each
  start, because a collector holds no durable state. It enrolls with the role
  token in `enrollment.roleToken`.
- **One port.** A collector enrolls on `head.listen`, the same port it uses
  after enrollment. That listener is mutual TLS, and it also accepts a client
  that shows no certificate. The head accepts `enroll-node` and the control
  operations, which carry their own credential, from such a client. It refuses
  `commit-batch`, `resolve-key`, `fetch-policy`, and `renew-node-certificate`
  from such a client.
- **The head's name.** A collector verifies the head against
  `installation.authorities` and the name `head.tallyowl.internal`, whatever
  address it dials. Thus a Service name, an IP address, or a load balancer all
  work.
- **The certificate.** The common name and a DNS name of each node certificate
  are the node name. A client checks the DNS name. The consensus sender check
  compares the common name. The recorded serial is the X.509 serial, in
  lowercase hexadecimal. A node name must be a valid DNS name, and
  `config check` refuses a `node.name` that is not.
- **The chain.** An issued chain is the leaf and the intermediate. The root is
  not in it.

The controller intersects the requested scope with the token policy. It does not
give a permission that is absent from the token.

The controller returns the effective role and location. It also returns the
applicable protocol and policy generations.

## 5. Capability negotiation

The node hello message contains:

- node ID;
- certificate serial;
- software version;
- supported CSIL protocol versions;
- supported segment versions;
- supported compression codecs;
- configured node roles;
- resource and storage capabilities;
- current policy generation.

The controller returns the permitted capability set. A node must not use a
capability that the controller did not permit.

This negotiation permits a rolling upgrade. It also prevents a token from
granting unsupported or unapproved behavior.

## 6. Certificate lifecycle

The certificate lifetime is `enrollment.certificateLifetimeHours`, 24 by
default. A role token can make it shorter with `certificate_lifetime_ms`, and
cannot make it longer. A node renews at two thirds of the lifetime, so it
continues through a head outage of one third of the lifetime: 8 hours at the
default. D62 replaced the earlier rule of renewal after 8 hours of 24.

The node uses its current mutual TLS identity for renewal. The head accepts a
renewal only from a verified peer whose node ID is the node that it renews,
and whose certificate serial is the current one of that node. The node does not
need the role token for a normal renewal. When a renewal is refused, or the
certificate has expired, the node enrolls again with the role token. Revoke the
role token to stop a collector that restarts.

There is no revocation list. The short lifetime is the revocation mechanism.

The controller can refuse renewal because of:

- node revocation;
- role-policy change;
- unsupported software version;
- incorrect cell or region;
- certificate misuse;
- an operator action.

Short certificate life limits the effect of delayed revocation data. It also
limits the value of a copied certificate.

## 7. Stateless nodes

A deployment can mount one reusable role token in many stateless pods. Each pod
gets a unique node ID and certificate.

An expired pod identity needs no manual removal. The head removes the node
record in its maintenance pass, after the certificate expired.

The token policy can limit active nodes. This limit prevents an incorrect
autoscaler from creating unlimited identities.

## 8. Stateful nodes

A storage process keeps its node ID on its persistent volume. A replacement
process can request a certificate for that node ID.

The controller verifies the storage role, volume identity, fencing epoch, and
existing placement before it accepts the request.

A new certificate does not give tablet ownership. The process receives tablet
assignments from the cell controller.

## 9. Kubernetes automation

A Helm installation can refer to an existing role-token Secret. A controller
administrator can also create a token for one Helm release.

Each pod uses the token only during enrollment. A collector keeps its private
key and certificate in memory, and enrolls again when it restarts. The
collector chart takes the token from a Secret, in
`deployment.tls.roleTokenSecret`, as an environment reference.

A deployment can retain the role token for future replicas. Certificate
rotation does not require a pod rollout.

The chart does not put a role token in a command line, log, or generated
manifest output.

## 10. Audit data

The audit record contains:

- token ID;
- assigned node ID;
- effective role;
- cell and region;
- certificate serial;
- request time and source;
- permitted and refused capabilities;
- renewal and revocation events.

The audit record does not contain the role-token value or private key.

## 11. Security tests

The test plan includes:

- repeated enrollment with one permitted token;
- token expiration and revocation;
- maximum-use and active-node limits;
- role escalation attempts;
- cell and region escape attempts;
- controller-voter enrollment attempts;
- tablet-placement attempts by a storage process;
- certificate renewal and cascade revocation;
- copied-certificate detection where possible;
- protocol downgrade attempts;
- audit-record verification.
