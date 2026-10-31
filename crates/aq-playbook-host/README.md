# AQ playbook owner foundation

An internal, unmounted embedding of the exact aq-playbooks source pinned in
`vendor/aq-playbooks-source.json`. No domain commands or events are registered.
The CLI and synthetic host are not dependencies. There is no automatic driver,
production registration, worker launch, supervisor, notification dispatcher or
public API in this crate.

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
