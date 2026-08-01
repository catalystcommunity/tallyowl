# Service conventions

Every TallyOwl service follows these conventions, in Rust, in Go, and in
TypeScript. They exist before the first crate, because the first
implementation otherwise becomes the standard by accident and the others pay to
catch up.

## 1. The language rule

**A person who does not operate TallyOwl must understand every message that
reaches them.**

An error, a health state, or a status on a screen reaches an executive, a
support conversation, or a customer email. None of those readers has a
glossary.

This rule covers what surfaces:

| Surfaces to a person | Stays technical |
| --- | --- |
| Error messages and codes | Internal storage terms such as tablet, virtual shard, and locator run |
| Health and readiness states | Log fields that only an engineer reads |
| Dashboard labels and warnings | Source code identifiers |
| Notification text | Metric names, which follow their own convention |

The test: read the message aloud to somebody who has never seen TallyOwl. If
they ask what a word means, the message fails.

## 2. Errors

### The shape

Every error carries a code, a message, whether a retry can help, and optional
detail. `csil/types/common.csil` holds the normative type.

### The codes

A code is a short phrase, not an abbreviation.

| Code | What a person understands from it |
| --- | --- |
| `invalid-argument` | Something in the request was wrong |
| `unauthenticated` | We do not know who you are |
| `permission-denied` | We know who you are, and you cannot do this |
| `not-found` | The thing you asked for does not exist |
| `already-exists` | You tried to create something that is already there |
| `resource-exhausted` | You have used up an allowance |
| `failed-precondition` | The system is not in a state where this works |
| `unavailable` | We could not reach part of the system |
| `schema-unsupported` | Your software is too old or too new for ours |
| `budget-exceeded` | The work cost more than the limit allowed |
| `incomplete-result` | We could not see all of the data |
| `internal` | We made a mistake |

`internal` says "we made a mistake" on purpose. An error that blames the caller
for a fault in TallyOwl wastes their time.

### The message

Write a message for the person who has to act on it.

- Say what happened, then what to do.
- Name the limit and the observed value when a limit caused it.
- Never include a secret, a credential, a payload, or personal data.
- Never include a stack trace. That belongs in a log.

Good: `Batch rejected. It holds 640 KiB and the limit is 512 KiB. Reduce the
batch size or raise the limit for this project.`

Bad: `ERR_FRAME_OVERFLOW: max_frame_bytes exceeded (524288 < 655360)`

### Retryable

`retryable` is a fact, not advice. Set it true only when the same request can
succeed later without a change. A caller builds automation on this field, so a
wrong value produces either a retry storm or lost data.

## 3. Health

Two states, and they answer two different questions.

| State | Question it answers |
| --- | --- |
| Live | Is this process working, or should something restart it? |
| Ready | Can this process do its job right now? |

**Readiness fails when the service cannot safely do its job.** A service that
cannot reach its durable store fails readiness. It never accepts data that it
would then discard.

Known cases where readiness must fail:

- ingest cannot reach its durable store;
- a forwarder has stopped its Corndogs timeout sweep, because retry and
  worker recovery stop with it;
- a query service cannot reach the catalog;
- the applied collection policy is older than the configured staleness limit.

A health response says which check failed, in the language of section 1. "Cannot
reach the durable store" beats "corndogs_conn=nil".

## 4. Logs

### What every log line carries

- the time;
- the severity;
- the service and its version;
- the workspace and project when the work belongs to one;
- the request or batch ID when one exists;
- the message.

### What a log line never carries

- a secret, a credential, a token, or a key;
- a request body, a header, or a cookie;
- personal data of any kind;
- an end-user ID at info level or below.

An end-user ID may appear at debug level for a support investigation, and the
project policy can forbid even that.

### Severity

| Severity | Use it for |
| --- | --- |
| Error | Something failed and a person must act |
| Warning | Something failed and the system recovered |
| Info | A state change that a person would want to know about |
| Debug | Detail for an investigation |

An error that the system retried successfully is a warning, not an error. A log
full of errors that resolved themselves trains people to ignore errors.

## 5. Configuration

### Sources, in order of precedence

1. a command-line argument;
2. an environment variable;
3. a configuration file;
4. the built-in default.

### One name for one setting

A setting has one key path, and that path is the same everywhere it appears. A
developer who reads a Helm value already knows the environment variable.

| Place | Form |
| --- | --- |
| Helm value | `storage.receiptPolicy` |
| Configuration file | `storage: { receiptPolicy: ... }` |
| Environment variable | `TALLYOWL_STORAGE__RECEIPT_POLICY` |
| Command-line flag | `--storage.receipt-policy` |

A section separator becomes a double underscore in an environment variable, so
a key that holds an underscore stays unambiguous.

The configuration file is YAML, and its tree is the chart values tree. A
rendered chart and a local file are then the same document.

**No service reads a `.env` file.** That convention belongs to a container
runtime, and it hides precedence. A container or compose workflow may supply
one, and it reads `.local.env`, which Git ignores. The committed
`.local.example.env` holds no secret and documents every key.

### Rules

- **Validate everything at startup, and refuse to start on a bad value.** A
  service that starts with bad configuration fails later and in production.
  Nobody connects that failure back to the configuration.
- Refuse a value that the deployment cannot satisfy. D4 requires this for
  `durable_copies`, and the rule is general.
- Never log a resolved secret. Log that a secret resolved, and from where.
- Every value has a default that is safe for a home installation.
- A configuration error message names the setting, the value, and a valid
  example.
- `tallyowl config check` resolves everything, reports which source won for
  each value, masks every secret, and exits non-zero on an invalid value.

## 6. Metrics

Metric names are the one place where technical language wins, because a metric
feeds a dashboard query rather than a person reading prose.

- lower case, with underscores;
- prefixed with `tallyowl_`;
- suffixed with the unit: `_seconds`, `_bytes`, `_total`;
- labelled with workspace and project where per-project cost matters.

A metric never carries an end-user ID as a label. That turns a metric series
into personal data and multiplies cardinality without limit.

Every service exposes the same instruments through its Prometheus and
OpenMetrics endpoint and through the native path. See D12.

## 7. Time

- Store and transmit milliseconds since the Unix epoch.
- Keep event time, receive time, and commit time as three separate facts. Never
  collapse them.
- A duration field ends in `_ms`.
- Display in the reader's timezone. Store in Coordinated Universal Time.

## 8. Review

A change that adds an error code, a health check, or a configuration value
updates this document in the same commit.

Read a new error message aloud before merging it. That check costs seconds and
catches most violations of section 1.
