# Actual canonical-data clone with an empty Playbooks owner

Passed on2026-11-01. This is a separately approved **stage**, not canonical
promotion. It restores a consistent snapshot of the existing canonical database
into a new database in the already owned private PostgreSQL Unix-socket instance.
The qualified stage is left running for root review at `http://127.0.0.1:4218`
(worker4219), PID895935 at this checkpoint. Canonical4188/4189 retains its original
state35 database, config, environment and binary. No canonical lifecycle or
configuration change was performed by this proof.

## Exact source, restore and configuration

Backend source `2baa8e7b387b52717ccc9b97a6b970b447cf319b`, default features with no
test-support, binary SHA256
`6f07dd8e98d3ae74e4886a6bf66b10b8f85a0a22c05756b77738543c7ec7da65`,
served the unchanged immutable dashboard manifest
`624707540cc6566158c3721e4733f69934c8a1670c4ffb972afb7d469fb8e663`.
The exact binary pin and source/test qualification are retained in
[the empty-owner source record](../empty-playbooks-owner-2baa8e7/README.md).

Source PostgreSQL17.11 is physical cluster `7702509189573074990`, database
`aq_test_d2942959309042a3ad3e087dec11e03e`. It is14.6MB, UTF8,
en_US.utf8 collation/ctype with libc2.36, postgres ownership/default DB privileges,
no DB-role settings, and only the builtin plpgsql extension. A repeatable-read,
read-only exported snapshot was held while collecting logical row fingerprints
and taking a custom-format pg_dump using that **same** snapshot. The private
0600 dump is547201bytes, SHA256
`6999eb1b0b2e14ef3816eb6c3b87c5d5cd3a389577e061c79766550b02186642`.
Its contents and credentials are not committed.

The NEW target database is
`aq_canonical_stage_49fac61d0492413b843281873437b0e7`, physical cluster
`7703296609124593685`, through the actual protected UDS. It was created from
template0 with the source's UTF8/en_US.utf8 properties explicitly preserved,
then restored with original owner/privilege semantics in one transaction. Before
AQ startup, **all109 original tables/215 rows had equal logical row fingerprints**
and every sequence matched. Row order and PostgreSQL MVCC tuple identity are not
part of those logical fingerprints. Source and target snapshot evidence is retained.
No existing target or inert-proof database was overwritten; no rows were cleared.
The target's capacity allowance and local transport checks passed before restore.

The actual retained Owner is `f4c33664-c524-4395-815f-5af6109a418b` and the
actual enabled project1→workspace3 binding is revision2. No fixture project,
Graph node, worker eligibility, verification, task or historical activity was
inserted. The new operator-selected namespace is
`2ae9605f-5277-4c58-aad8-61ad8121fcac`, generation1; it was absent before startup.

The stage config changes only server listeners/public origin to4218/4219 and adds
the explicit human_review_v2 owner with `deployments=[]`, a fresh protected
namespace and one-time private bridge/engine keys. All original non-server config
values compare equal after parsing, including auth, agent/Graph and terminal
settings. `AQ_PRIVATE_VALUE_KEY_K1` is retained privately for existing protected
values; its bytes are neither printed nor committed. Both core SQLx and engine
connection files target the same physical UDS database. Config SHA changes from
`de60d617b02dbf7b2eac4c25c425239a5f85479a8d1d113f93996b559864d45e` to
`fc04681458f829591704bef9041e18ad04acdd01b9190b6b925d5831340e1d68`.
No artifact, binding, grant, recipient or expiry is fabricated to populate the UI.

## Actual startup, reads and graceful ownership handoff

Actual default-backend startup advanced the **target only** from AQ35→37 and
created component schema4. Current Definitions and Activations return authoritative
successful empty pages, with `current_namespace_state`, current_catalog=true and
the actual namespace identity. There are zero grants/artifacts/activations and
no configured deployments. The unchanged historical startup profile remains
unmounted, so loaded-definitions capability is false. Actual Runs/admissions for
this new namespace are empty. Decisions read actual retained review data; no
baseline review was deleted to force an empty page.

Actual core (`aq-canonical-stage-core`), bridge and engine application names
appear in the target's pg_stat_activity with Unix client addresses and the same
database. The physical system-ID and peer/socket checks are independent operator
observations, not additional AQ loader guarantees. Synthetic forwarding and
browser interception are absent.

Graceful shutdown released the first qualified owner's connections/listeners;
restart acquired the same retained namespace with incarnation3→4 and the same
keys/config. The final owner is intentionally left running for review.
`diagnostics.json` truthfully records earlier operator-glue refusals: a missing
stdin attachment before backup; a malformed stage core URL rejected by peer auth;
and two SQL-output parser failures that each shut the actual owner down cleanly.
Those diagnostic incarnations1/2 were not reset or hidden by restoring data.
There were no product source, authority or socket-policy changes to correct them.

All17 work/effect counters match their restored baseline: **one existing execution
attempt**, zero Playbooks admissions/effects/cancellations/outbox, human/agent Graph
operations/outbox, mailbox, notifications, reviews/decisions and observations.
This is preservation, not a claim of an all-zero database.

After migrations/startup/restart,104 of the109 original tables remain logically
fingerprint-identical. The five expected changed tables are:

- meta and schema_migrations: AQ35→37 plus runtime owner metadata;
- playbook_namespaces: the new actual empty owner;
- notification_budget: dispatcher incarnation metadata; no messages/deliveries;
- terminal_sessions: only runtime_id differs in the clear header. Its ledger7,
  authority revision, observed_at, last output, pending bytes0 and operation count4
  remain unchanged. No authority, timestamp or grant is extended.

The actual principal's id/issuer/subject/role/email remain equal. PostgreSQL restore
changes xmin/MVCC identity, and bootstrap authorizationRevision **does change**.
The new authorization context is required; this is not a rebound principal or a
renewed grant, nor a claim that full bootstrap objects are byte-identical.

## Actual compiled browser and existing feature regressions

The empty-catalog and all-page GET-only probe passed **13740ms** at1440/390/320.
Definitions/Activations show successful empty current metadata and survive reload;
startup-loaded capability remains false. No install/activate/create-policy action
is exposed. Catalog navigation performs zero unrelated engine/history reads.
Overview, Work, Knowledge, Agents, Supervisor, Results and Settings shell/data
reads passed at each width with zero browser mutations, foreign requests, page
errors or failed API responses. Captures were inspected for readable empty state,
roster/project-selection behavior, stale-session truthfulness and reflow.

Separate existing probes against the same stage/bundle passed:

- Actual Work/Graph/Knowledge:3341ms,16 human API reads, three task/two knowledge
  records, exact pinned document read and a real two-node/one-edge dependency
  graph, zero mutations. Desktop and phone workspace/document geometry passed.
- Actual Overview task contribution:532ms, complete recorded-task observation
  with one draft/two active records; no running/readiness/capacity inference.
- Actual Supervisor:1536ms, existing mailbox read, zero enqueue, retained runtime
  unavailable, draft/scope/composition/phone geometry checks. No provider or worker
  execution or fabricated agent reply.

The complete successful owner lifecycle including the browser hold took53358ms;
this is an integration observation, not a throughput benchmark. Results, probes,
safe API observations, fingerprints and selected captures are retained with
SHA256SUMS. Private connection/key/env/config contents and backup bytes are excluded.

## Cutover preflight: retained uncertainty is not cleared

A read-only process scan found an older same-database AQ preview PID675372 on
4196/4197, in addition to canonical874715 on4188/4189. Root separately retired that
owned older process gracefully. The subsequent scan finds only canonical874715
with the original database pointer. Final quiescence must still stop that process,
verify source SQL clients are drained and prevent further writes before the final
snapshot. The stage copy is not a live replica or automatically current.

The source has one **unresolved held task claim**: execution_attempts says offered
and holds_task=true, while profiled_authority_hot says finished. It also retains a
phase execution grant, started receipt and an expired material lease. There are
zero unexpired material leases, zero recently heartbeating nodes, no worker TCP
connections, no pending terminal bytes/controller/outbox, notifications or source
transfers. These observations do not prove physical process stop or release the
held claim. Exact attempt/grant details stay private.

The approved disposition is **RETAIN UNRESOLVED UNCHANGED**, with no new attempt,
release, reconciliation, authority extension or timestamp restamp. Relocation
preserves that hold and all fences. Source-backed startup behavior supports this:
profiled_execution::policies::install is a no-op without its config; expiry::run
exits without that config; main starts no other automatic attempt scheduler or
reissuer. Empty Playbooks has no grants/artifacts/effects and its review/event
scans join this namespace. Actual startup+restart leave nodes, execution_attempts,
profiled_authority_hot, material leases, phase grants, started/closure/receipt
history and all other unchanged source tables logically identical.

A final quiesce/fresh backup/NEW restore and explicit canonical promotion decision
remain separate. No claim here says canonical optional owners are enabled.

## Later composed central-admission union, same stage and assets

Root's next qualified default37 binary, source
`511ee995ac55dd6345590732705b5a3bfde5130d` (runtime stamp511ee99), SHA256
`17698c625675b7a994b0a56bbd37737becbf039380093ae9dd32fe1bdcd27b25`,
now supersedes6f07 for this stage. It includes the qualified central-admission
0.1.4 source union. No central host or profile is started by this check.

The exact owned stage process was gracefully stopped, releasing owner4 and all
its clients/listeners. The new binary started with unchanged config, environment,
private keys, physical UDS DB and624 assets and acquired owner5. Protected current
catalog/capability/Overview/Supervisor/Graph GET checks passed. The same-DB bootstrap
principal object is unchanged for this swap; the earlier restore's expected MVCC
revision change remains distinct. All17 work counts and the unresolved held claim
remain unchanged; a repeat109-table fingerprint check still finds exactly the same
five expected metadata/runtime tables differing from the original source snapshot.
All critical node/attempt/hot-authority/material/phase/receipt/closure/release tables
remain identical to that original snapshot. No authority is repaired or reissued.

The unchanged frontend's full13.740s visual suite was not repeated for a backend-only
swap. The later bounded actual API/owner proof is under `union511ee99/`. The stage
is left running on4218/4219, now PID898833 at this checkpoint, for root review.
Canonical4188 still uses its original backend/database. The final cutover proposal
uses this later qualified binary and requires a fresh quiesced source snapshot.
