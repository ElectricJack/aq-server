# Implementation decisions

This document records the concrete choices for the [Rust design](0001-aq-playbooks.md). The research catalog supplies scenarios, not legacy compatibility requirements. The five-crate workspace is independently buildable; the [AQ host integration boundary](../aq-host-integration.md) distinguishes component functionality from live product integration.

## Artifact and language

Version 1 uses Markdown beginning with a `---aq-playbook` JSON header and a separate JSON `CompileInput` bundle. The compiler never calls a model. Source normalization converts CRLF/CR to LF and preserves other bytes. Source, captured definitions, interfaces and executable contracts are hashed; provenance timestamps and model conversations remain outside executable identity.

The bounded schema subset contains closed objects, strings/enums, integers, booleans, arrays, null, nullable and finite unions. Decimal quantities use declared strings. Unknown values remain distinct from null and false. See [the portable format](../../crates/aq-playbooks-model/FORMAT.md) for canonical bytes and evaluation limits.

Dependencies select either an exact artifact or a statically named compatible callable catalog binding, optionally restricted to an explicit artifact allowlist. Root admission checks interface variance, current authority, scope, availability and cycles against one catalog revision, then captures the entire transitive closure. Subsequent activation never retargets admitted descendants. Catalog history needed by blocked event destinations is retained.

Join `member_keys` assigns stable declared names to participants while retaining each actual run/operation identity in its result. The compiler tracks heterogeneous participant output types and narrows accepted results on successful edges. Legacy unnamed joins preserve identity-keyed inspection results. All/any modes, deadline and remaining-owned-work policy are explicit.

Serial foreach captures its finite collection once, uses a backedge to advance, restores lexical bindings and shares cumulative budgets. Parallel work uses owned async child/operation handles and explicit joins, as permitted by the design's initial parallelism model. Shared-variable parallel foreach, recursive/dynamic calls and active-run migration remain explicitly unsupported design extensions.

Fixed interval planning is a pure model helper with stable occurrence IDs, bounded catch-up, coalescing and skipping. A host atomically stores its cursor and outgoing occurrences before delivery. The runner persists delay/wait deadlines. Zoned calendar arithmetic remains host-owned.

## Atomic storage, scheduling and inspection

Every mutation compares and swaps a namespace revision. Snapshots, receipts, event/wait changes, child creation, budgets, outgoing intents and index updates commit together. External dispatch happens after commit. PostgreSQL session advisory locks plus durable owner/incarnation/backend identity enforce one writer; they do not fence already-issued effects in a partitioned external system.

The authoritative snapshot is accompanied by durable ready, timer, effect, correlation, trigger, child-dependency and receipt indexes. The bounded driver rotates scopes and work classes, including one-transition batches. Wake hints are optional: persisted indexes recover progress after restart. PostgreSQL mirrors indexed discovery and safe public inspection rows in the same transaction, applying changed-row/deletion deltas rather than replacing unchanged index history. Inspection uses bounded keyset pages without decoding a whole protected snapshot.

The PostgreSQL correctness boundary still writes a whole namespace snapshot. State-size limits bound its cost; indexes improve discovery and public paging without claiming unlimited namespace scale. Admission/storage benchmarks name their workloads and hardware rather than adopting provisional release thresholds by assumption.

Operational telemetry uses fixed-size histograms and value-free counters exposed through `Engine.metrics.snapshot()`: initial admission-to-first-visit queue delay, transition evaluation, storage load/commit, host call time, admission attempts/refusals and duplicate events. These process counters reset when an Engine is constructed; the host exports them to its metrics backend. `StateMetrics` and CLI `metrics` explicitly scan retained state for open waits, outstanding/uncertain commands, child depth, retained bytes, queued-root/recovery ages and command admission/completion timings. Retained-history measurements are not lifetime counters. This diagnostic scan is separate from indexed scheduling and inspection.

## Retention and replay

Retention policy specifies event replay, terminal-run and audit horizons plus a tombstone quota. Garbage collection examines bounded pages, preserves unsettled obligations, retained evidence correlations, active/blocked destination manifests and the complete artifact closure of active bindings. Terminal cohorts can be collected across pages. Disposal records and address checks prevent replacement under a retained immutable digest.

Expired accepted events leave replay tombstones; unchanged or changed content under an expired identity cannot silently create new runs. An authenticated producer epoch fence provides explicit retirement of old delivery generations and safe tombstone compaction. The host must persist the epoch with its delivery record and must not relabel an old retry as a new generation. General waits whose required correlation history has expired are refused visibly. Quota pressure never evicts unresolved work.

## Protected persistence

A host-supplied `ProtectionKeyring` enables authenticated XChaCha20-Poly1305 encryption of snapshots and artifact bytes. Random nonces and authenticated namespace/revision/address/key identifiers detect tampering and cross-context substitution. Keys are never written to the database or public diagnostics. Missing/wrong keys and plaintext downgrade attempts fail closed.

The CLI accepts an explicit owner-only JSON keyring file through `--protection-keys`; it does not generate or assume production keys. Rotation writes with the new active key while retained old keys decrypt historical backups. Protection of previously plaintext data cannot retroactively encrypt old WAL or external backups. Public metadata includes opaque identities, status, scheduling fields and declared safe receipt projections; raw inputs, outputs and free-form diagnostics remain private. Unknown receipt kinds/fields default to no public values.

Without a configured storage protection facility, runs whose pinned closure declares sensitive command inputs or outputs are refused before input persistence. Event contracts can declare `sensitive_payload_fields`; those events, known protected async completions, and protected trigger inputs are refused before event persistence. An artifact or saved state cannot grant that facility to itself: each transaction derives availability from the actual backend.

## Administration and host integration

Standalone administration requires an explicit database and namespace. Writes acquire exclusive ownership. Activation controls separately govern new calls, new event starts and existing execution authority. Capability revocation is checked when effects/children begin without replacing the original caller or its ceiling. CLI recovery requests record actor, reason and evidence; they do not attest external success or authorize an unsafe replay. Registered hosts perform truthful readback and cancellation.

The explicit synthetic AQ catalog has twelve commands, five context views, nine work templates and six work-change variants. Owned `work.ensure` acceptance, terminal completion and substantive disposition are separate observations. A model assessment or completed review cannot substitute for human decision authority or executed check evidence.

The coding pilot requests distinct bounded repair attempts and observes the authoritative destination again before rebuilding each candidate. A fresh project observation opens with an empty snapshot selector; its resulting named snapshot is immutable. The watcher uses host-attested complete coverage and pre-scan episode revisions. The report freezes its context, preserves one notification identity, and settles/cancels author work while retaining late results.

Actual AQ services and deployment wiring are external responsibilities documented in [the integration boundary](../aq-host-integration.md). This repository's stateful fake host and fixtures test the contracts without importing sibling private state or inventing production endpoints.

## Review and verification

Three GPT-6.1 Sol agents implemented and independently reviewed the components, with supervising integration reviews. Regression cases include artifact verification bypasses, heterogeneous Join typing, source/catalog pinning, request acceptance versus completion, deadline races, unknown outcomes, protected projections, revocation, one-item scheduling starvation, transitive retention, bounded GC and database/process crash boundaries. [Acceptance evidence](../acceptance.md) records the current tests and measurements.
