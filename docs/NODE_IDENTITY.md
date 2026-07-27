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

The first certificate lifetime is 24 hours. Renewal starts after 8 hours. Both
values are configurable.

The node uses its current mutual TLS identity for renewal. It does not need the
role token for normal renewal.

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

An expired pod identity needs no manual removal. The controller removes it after
its lease and certificate safety periods.

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

Each pod uses the token only during enrollment. The pod keeps its private key
and certificate in a memory or protected runtime volume.

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
