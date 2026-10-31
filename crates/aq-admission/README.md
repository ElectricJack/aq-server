# AQ admission contracts 0.1.5

Pure versioned preparation contracts and bounded placement proposals. This crate
contains no daemon, database, network client, provider call or secret delivery.
Types and structural validation confer no authorization, reservation, retention
or stop proof. Runtime owners authenticate facts and enforce the final transaction.

`MaterialLease` binds the existing session `LaunchBinding` and exact credential
profile/revision/provider/account/endpoint/purpose, bytes/digest and bounded time.
It is private authenticated delivery metadata, never a prompt/public profile.
Only the real owner may issue/rotate/revoke it. Byte verification is mandatory;
worker presence alone does not establish account identity. `MaterialReceipt`
identifies an attempt-private binding artifact, not a filesystem path or launch
grant. Provider material and delivery routes are not installed by this package.

`SourceAdmission` requires explicit immutable object or fully qualified safe ref,
exact public repository generation, source-policy artifact and bounded original
observation time. `RetainedArtifact` verifies exact bytes/digest/size and retention
metadata; its receipt reference still needs authenticated owner verification.
No placeholder source/outcome/context artifact is synthesized. The initial helper
caps the total retained descriptors at 128, including definition references; it
can refuse a public-envelope-valid manifest with 128 distinct context artifacts
plus definitions. This is an explicit smaller admission profile, not full maximum
public-envelope support. Callers must refuse the unsupported larger profile.

Placement proposes bounded per-project round-robin assignments, preserving
priority and stable ties. Resource fit precedes truncation, and earlier oversized
work cannot hide a later fit. Warm source state is only a preference among
eligible nodes. Cold eligible nodes may prepare. Expired/foreign-boot capacity
is not used. Proposals explicitly carry `authority_admission:false`; the daemon
must independently claim and fence actual resources and worker admission.

`artifacts/contract.schema.json` and `fixtures.json` are independently packaged
publications with drift tests. The pinned shared launch fixture is copied from
session protocol 0.1.0 and is not a new version of that protocol. Schemas include
field/array/UTF-8 bounds; Rust validation additionally checks relational identity,
time/freshness, role, object-format, capacity and scope invariants. No schema can
establish live authorization or retention. Receipts and leases need real owner
records, not caller assertions.

Run `cargo test -p aq-admission --locked --offline`. For standalone package tests,
provide the separately pinned public `aq-session-protocol =0.1.0` artifact through
Cargo's normal registry/patch mechanism; no sibling worker source checkout is
needed. In this development workspace, packaging uses the retained public vendor:

```sh
cargo package -p aq-admission --allow-dirty --offline \
  --config 'patch.crates-io.aq-session-protocol.path="/absolute/public/vendor/aq-session-protocol-0.1.0"'
```

Publication generation: `cargo run -p aq-admission --example export -- schema`
and `... -- fixtures`. New incompatible fields/semantics require a new immutable
contract version and coordinated consumers; do not edit pinned session bytes.

The additive 0.1.1 `transport` module publishes separate
`artifacts/transport.schema.json` and `transport.fixtures.json`; the original
preparation publication remains byte-identical. These envelopes describe future
handlers. Register/poll/accept/prepare/grant/renew and private material transfer
still require authenticated assigned-worker admission by the actual daemon.
PreparationPlan permits preparation only; it is not a launch grant. The initial
transport frame is capped at 1 MiB, with at most 8 offers, 16 material leases,
32 reconciliation records and 128 artifact descriptors. Larger otherwise-valid
shared manifests must be refused under this narrower profile.

The server recomputes remaining delivery TTL on every replay from the same
absolute expiry. Workers charge the entire request interval against that TTL.
A retry never restarts execution, extends a reservation, or extends a grant.
Stop evidence requires the qualified assigned host's durable guardian journal;
NotStarted closes an exact durable no-launch reservation, and Unknown never
satisfies release. The pure journal ledger detects changed replay and monotonic
conflicts; it does not prove remote process stop. Central source/effect owners
must separately verify their actual release receipts before claim release.

OfferedLaunch carries the exact lifecycle Binding. Its lineage UUID is UUIDv5
under frozen namespace `43c0fccf-eb6e-5357-949c-0bc68fbf51ea` with raw 48 network-order
UUID bytes project, workspace, task. Independent Python vectors are packaged in
`artifacts/task-lineage-v1.json`. Server task aggregate identity owns the mapping;
a worker-supplied lineage never resets retry/time/cost accounting.

The additive 0.1.2 `execution_spec` publication defines the actual worker's
secret-free `ProfiledExecutionSpec` artifact. The producer writes its canonical
bytes and seals that digest in ProfiledOffer. It records the exact lifecycle
binding, graph CAS version and independent content revision, source admission,
source/outcome/API01 graph-profile refs, task observation and required startup
task context, and sealed selection digest. Its manifest comparison checks all
shared bindings and the digest without a self-reference or manifest digest
cycle. The task observation and context remain retained Context artifacts, and
the spec is a retained Composition artifact. Every byte transfer still requires
actual descriptor byte/digest verification and exact authenticated attempt scope.

`execution-spec.schema.json`/`execution-spec.fixture.json` are separate from both
prior publications; examples are synthetic conformance data. The spec decoder
caps the artifact at 256 KiB and rejects duplicate/unknown fields. Graph
`task.version` is the shared manifest's `task_revision` (the admission CAS fence);
`task.content_revision` remains independently frozen. Central execution requires
its distinct future authority adapter and is refused by this worker spec.

## Additive host-start contract (0.1.3)

`started::Request` and its separate schema bind the original durable start
operation, exact launch/grant/logical reservation owner, qualified runtime and
journal to an actual `aq-session-host` Running observation. The pre-grant logical
guardian ID is distinct from the physical guardian nonce allocated at launch.
Existing preparation, transport and execution-spec document bytes are unchanged.

The 16 KiB decoder rejects other host phases, missing panes, contradictory stop
evidence, changed observation digests and foreign launch bindings. Canonical
digests have independent Python vectors. These checks do not prove that a remote
process exists: the daemon must authenticate the current admitted node/runtime,
check the actual prepared journal head and central grant, and trust only its
qualified worker owner. Deserializing a report is never execution authority.

`started::Receipt` is an immutable historical acknowledgment. Replaying it cannot
renew permission, extend a deadline, revive a stopped host or authorize terminal
input. Worker integration must persist the original operation and genuine host
observation before sending; renew counters and parser events are not start proof.

## Additive post-Started stop contract (0.1.4)

`stopped::Request` carries the existing worker stop envelope plus the exact
serialized host StoppedProof, runtime qualification and reservation. Its separate
schema preserves every earlier transport/Started artifact. The bounded request
requires a stopped phase, ECHILD, retained pane identity, root wait status and a
positive reaped count. `validate_after` binds the full physical guardian, launch
configuration and pane to the immutable original Started report. A newer permit
sequence may have a shorter deadline; the same sequence cannot change it.

Decoding is structural validation, not kernel attestation. The authenticated,
qualified worker must obtain the actual opaque host proof and durably journal it.
The daemon authenticates the original owner and records the receipt/journal/hot
transition atomically. Expired execution can report stop; stop does not release
the task claim, source or effects, nor does it establish task success. Historical
receipts cannot renew execution. Pre-start loss and changed-boot recovery remain
separate protocols. Independent Python canonical vectors cover the fixture and
Unicode values; actual tmux serialization qualification is recorded separately.

## Pinned reviewed source expectations (0.1.5)

`outcome::ExpectedOutcomeDefinition` is the initial finite pre-execution outcome
schema. It pins the exact project/task/source and authorized context references,
bounded acceptance criteria, validation policy, mandatory human-owner or policy-
actor review, and named validator requirements. It contains no attempt/spec hash
and can be retained before composing `ProfiledExecutionSpec`.

The definition's content digest is the spec's `outcome_artifact` and manifest's
outcome binding. `validate_against` enforces exact linkage, including graph CAS
and content revisions; unknown kinds and review shortcuts refuse. This pure API
does not execute checks, authenticate a reviewer, accept a result, release source
or complete a graph task. Those require the daemon's actual evidence owners and
current revision-bound policy evaluation.0.1.4 stopped archives remain unchanged.
