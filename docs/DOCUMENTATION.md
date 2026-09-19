# Documentation language

## 1. Standard

Use ASD-STE100 Simplified Technical English for all technical documentation.
Use Issue 9, dated 2025-01-15.

The official standard has writing rules and a controlled dictionary. Use the
official standard as the primary reference:

<https://www.asd-ste100.org/>

## 2. Project rules

Apply these rules:

- Use an approved dictionary word or a defined technical term.
- Use one term for one meaning.
- Do not use a synonym to add variation.
- Use American English spelling.
- Use the active voice. Use the passive voice only if the agent is not known.
- Use simple present, simple past, or simple future tense.
- Do not use a complex verb construction.
- Use a verb to identify an action.
- Write a short sentence.
- Use a maximum of 25 words in a descriptive sentence.
- Use a maximum of 20 words in a procedural sentence.
- Write only one instruction in each procedural sentence.
- Give information gradually.
- Put only one subject in each sentence.
- Put only one topic in each paragraph.
- Use no more than six sentences in a paragraph.
- Use a vertical list for complex information.
- Do not omit necessary words.
- Do not use a contraction.
- Define an abbreviation at its first use.

## 3. TallyOwl terms

The following words are TallyOwl technical nouns:

- end-user ID
- app driver
- assist
- attribution model
- batch
- browser package
- campaign
- cell
- channel
- cold tier
- collector
- controller
- conversion
- durable copy
- envelope
- environment
- exact index
- fencing epoch
- global directory
- head
- hot tier
- ledger
- locator run
- microsegment
- origin
- policy generation
- project
- property
- projector
- quarantine
- receipt
- reference application
- role token
- rollup
- segment
- source
- tablet
- tombstone
- touch
- virtual shard
- warm tier
- watermark
- workspace

The following words are TallyOwl technical verbs:

- acknowledge
- compact
- enroll
- erase
- ingest
- project
- replicate
- shard
- stamp
- sweep

Terms that this project does not use:

| Do not use | Use |
| --- | --- |
| adapter, backend adapter, host backend adapter, adapter library | app driver |
| actor, actor ID | end user, end-user ID |
| storage cell | cell |
| tag, app tag, operator tag, attribute | property |
| touchpoint | touch |
| SDK, client library | app driver or browser package |

Use "regional cell" where the region matters. Use "cell" everywhere else.

The word "adapter" keeps its ordinary meaning for a compatibility translator,
such as a PromQL adapter. It does not name a TallyOwl client component.

The envelope field name `sdk_name` stays as written, because a wire field name
is not prose.

An **end user** is a person or account that a customer's application
identifies. An **operator** or a **member** is a person who signs in to
TallyOwl. The two never mean the same thing.

The campaign terms have one meaning each:

| Term | Meaning |
| --- | --- |
| **campaign** | A named marketing effort. A published link carries the name, so TallyOwl can tell which effort brought a person |
| **touch** | One arrival from outside: somebody followed a link, or opened the address directly. One record |
| **channel** | The kind of traffic a touch was: paid search, organic search, social, paid social, email, display, affiliate, referral, direct, or other. TallyOwl derives it. A producer cannot send one |
| **conversion** | The outcome a customer measures: a purchase, a sign-up. It carries an exact decimal value |
| **attribution model** | A rule that decides which touches earned a conversion and how much of its value each one gets |
| **assist** | A touch that was inside the window and took no credit under the model that was applied |

Use **touch** and never "touchpoint". The two named one thing.

Every message that reaches a person uses plain language. See
[CONVENTIONS.md](CONVENTIONS.md) section 1.

Use each technical term with one meaning. Add a term to this list before you use
it with a new project-specific meaning.

## 4. Abbreviations

Define an abbreviation at its first use in each document. This table gives the
approved expansion. Do not use a different expansion.

| Abbreviation | Expansion |
| --- | --- |
| CA | certificate authority |
| CBOR | Concise Binary Object Representation |
| CDDL | Concise Data Definition Language |
| CI/CD | continuous integration and continuous delivery |
| CSIL | CBOR Service Interface Language |
| DOM | Document Object Model |
| HTTP | Hypertext Transfer Protocol |
| IO | input and output |
| KV | key-value |
| mTLS | mutual Transport Layer Security |
| OTLP | OpenTelemetry Protocol |
| RP | relying party |
| RPC | remote procedure call |
| SBOM | software bill of materials |
| TCP | Transmission Control Protocol |
| TLS | Transport Layer Security |
| UUID | universally unique identifier |
| WAL | write-ahead log |

Do not define an abbreviation that the reader does not need. Prefer the full
term when a document uses the abbreviation only one time.

## 5. Review

Review each changed document before you complete the change. The review must
include terminology, sentence length, voice, abbreviations, and paragraph
structure.

Apply these rules as you write. The project has no automated checker and no CI
job that enforces them. The project owner reviews the documents periodically.

The repository can add a checker later to make the work easier. A checker finds
mechanical violations only. It does not certify correct ASD-STE100 usage, and
it does not replace a technical review.
