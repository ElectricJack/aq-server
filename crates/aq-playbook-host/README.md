# AQ playbook owner foundation

An internal embedding of the exact aq-playbooks source pinned in
`vendor/aq-playbooks-source-0.1.3.json`. By default, no domain commands or events
are registered. Explicit AQ adapters remain the embedding owner's responsibility.
The CLI and synthetic host are not dependencies. There is no automatic driver,
production registration, worker launch, supervisor, notification dispatcher or
public HTTP API in this crate.

`Owner::start` creates one dedicated synchronous OS thread. Its factory creates
storage/connections and performs migrations on that thread. PostgreSQL owns its
`aq_playbooks` schema and session advisory namespace lock; AQ supplies a stable
namespace and a new UUID incarnation. Reopening with another live connection is
refused. MemoryStorage is for tests/development, not cross-process fencing.

Requests and response serialization have bounded envelopes (default256KiB,
hard1MiB). Work/control queues are separate (default32/8, hard128/32). Tick is
explicit and bounded (default8 transitions, hard64); there is no unbounded polling
loop or waiter registry. Control has priority **between** synchronous engine calls.
A stuck call cannot be interrupted. Timed-out admitted requests return Unknown;
they can still commit. In particular, raw Admit has no caller idempotency key and
must not be blindly retried. The next AQ-owned durable admission bridge must add
that contract before mounting a mutation API.

Drop never joins on an async executor or creates a replacement owner. A blocked
thread retains its storage/ownership until its actual call returns. Stop reaches
a call boundary, releases the namespace, and retires the queue; it proves no
external process stop. All domain effects remain unavailable. Last-handle drop
abandons pending queue replies conservatively and releases only after an active
call returns. The operator must not replace a stalled owner by assuming timeout
is proof of cessation.

For protected PostgreSQL, construct the authenticated TLS client inside the
factory, with its own connect deadline, then call `protected::storage`. It requires
a keyring and finite statement/lock deadlines (1ms–30s, lock<=statement), configured
before schema work. The host supplies verified TLS for remote DBs; this crate does
not turn a caller-created client into TLS. Disposable loopback tests explicitly
use NoTls. A production startup adapter/configuration is still pending.

`read_keyring` accepts a <=4096-byte regular file owned by the current uid, with
no group/other permission bits, opened no-follow/nonblocking. Use an operator-owned
private directory, never an agent-writable root. JSON has `active_key_id` and
`keys` (1–16 key-id→64-hex-character values); original key IDs and retired keys
must be retained for existing ciphertext. Key bytes and connection strings are
not exposed in inspection. No plaintext mode is silently selected by the protected
factory. The general storage factory exists for tests and explicit host adapters;
calling it does not attest production protection or authority.

Tests use synthetic pure artifacts and disposable PostgreSQL, including protected
envelopes, wrong-key refusal, owner exclusivity/restart, exact artifact integrity,
queue overload, unknown timeout, and nonblocking drop. The separate aq-server
fixture holds synchronous initialization while actual HTTP readyz/renew/cancel
operations progress. It does not qualify a native harness or production host.

PB04's 25ms budget remains unmet: transition count bounds do not imply wall-time
bounds, and retained namespace snapshots/indexes still have measured history cost.
See `docs/implementation/AQ-12-through-17-plan.md` for the twelve command owners,
durable outbox/recovery and later integration gates.

## Explicit registered owner (0.1.1)

The default `Owner::start` still registers nothing. `start_registered` accepts a
trusted exact finite registry and an owning DomainHost factory after actual
engine namespace acquisition. It supports only the twelve specified command
names and reconcilable effects; installation/restart rejects changed or unknown
contracts. This is not a wire-supplied authority permit. The concrete AQ service
adapter lives in aq-server and currently registers only message.send.

The0.1.1 version consumed the separately qualified exact a463e59 source publication
(runner/Postgres0.1.1, unchanged model0.1.0); the original42/0.1.0 vendor source
remains retained unchanged. `AdmitOnce` uses immutable engine receipts, while
historical Admission lookup cannot create a run or renew authority. Explicit
Reconcile and Cancel Host modes separate lookup/stop from fresh dispatch. Public
inspection uses the existing bounded sanitized projection. Event acceptance,
fanout and run progress remain separate operations and acknowledgements.

## Confirmed admission recovery (0.1.2)

Host0.1.2 consumed the separately versioned exact d3bd349 source
archive (runner/Postgres0.1.2, unchanged model/compiler0.1.0). The old host1.1
source and engine archives remain immutable. Confirmed Unknown→Accepted resumes
the same blocked invoke before final completion; cancellation and authority
checks remain separate. No automatic repair of old already-stranded1.1 states or
online mixed-version namespace adoption is qualified. AQ uses a deliberately new
namespace/source pin for this runtime; existing origins are not restamped. This
is a correctness fix, not PB04 performance closure.


## Current catalog inspection (0.1.3)

Host0.1.3 consumes exact component source0173673 (runner/Postgres/CLI0.1.3,
unchanged model/compiler0.1.0). The new additive
`Owner::inspect_catalog(CatalogQuery, deadline)` sends a private typed read message
through the existing owner thread and delegates to that Engine's actual storage.
Existing public Request and Error enum variants and serialization are unchanged;
`CatalogCallError` preserves typed catalog conflicts/unsupported results without
adding a legacy wire operation. Reads use the bounded work queue and cannot
starve stop/cancellation control priority.

A single actual bounded query after Engine acquisition establishes
`Owner::catalog_supported()`. The result metadata is discarded. The support flag
never claims cached current contents or later health; bootstrap need not query
the engine. Later ownership loss, unsupported profiles or corrupt projections
remain errors, never empty lists or startup-configuration fallbacks. Stop clears
support before its acknowledgement. No State snapshot fallback is permitted.

Component migration4/writer-format2 fences previously opened legacy writers.
Retire a prior owner before deployment; do not apply this candidate to an active
legacy database as a metadata preview. The consumer source qualification and
no-canonical-change boundary are tracked in
[API-05 current catalog](../../docs/implementation/API-05-current-catalog.md).
