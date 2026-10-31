# AQ-13 first bridge and mailbox adapter checkpoint

This checkpoint implements the first scoped message.send bridge, not all AQ13
work/context/operation variants. Other command names remain unavailable. The
normal daemon builder mounts no playbook routes; optional explicit builder
injection supports a preconfigured protected policy. Automatic daemon startup
configuration, arbitrary artifact/activation HTTP mutation, full workload
capacity and the25ms PB04 budget are not claimed.

The new exact playbooks source publication a463e5918f914c8365a04dfe1b6778831cfebb95
has archive SHA25648f19b80fc0734c403ef04e7181d570b9fe675597f35caf08f0b94571ca8cc70.
Its242 source files are independently inventoried alongside the unchanged235-file
original42 source. Runner/Postgres/CLI use new0.1.1 identities; model/compiler
remain unchanged0.1.0. The actor uses only model/runner/Postgres, with no CLI or
fake-host dependency. The drift checker needs no sibling checkout.

AQ owns namespace/grant and protected immutable manual-admission inbox, effect,
cancellation and outbox records. The engine owns workflow state and creates
manual run+key receipt in one CAS. AQ ACK follows separately: an engine commit
with a lost AQ ACK returns the same historical run after restart and grant
revocation, without granting new execution authority. Unknown first admissions
are retained/held rather than silently retried or starving later pending work.
Recovery with no retained owner result remains unknown and creates no write.

A central runtime revision is captured before actual engine ownership. Adoption
of the acquired incarnation CASes that exact revision. Domain transactions check
the current runtime, locked grant and owner role; late old initializers cannot
overwrite a new incarnation. Clock expiry is rechecked after mailbox lock waits
and before commit. Revocation/retirement linearizes through the same namespace
row, never through a guessed thread timeout. Drop/timeout does not replace the
owner. Current source requires protected storage before starting this driver.

The message adapter calls the existing typed AQ11 owner-send service inside its
transaction. It derives immutable operation/message IDs and sender from the
retained runner intent and granting owner, enforces exact task/session recipient
scope, then commits mailbox acceptance, bridge receipt and protected fixed
producer outbox together. It does not create another mailbox, selector, graph
proxy or sender credential. `queued` is mailbox acceptance, not node/harness
receipt or human reading. Cancel before first owner admission retains a no-effect
tombstone; queued mail is too late to unsend. Exact historical lookup can resolve
both retained mail and confirmed pre-admission cancellation after grant expiry.

One manual driver quantum checks at most2 admissions and2 outbox events, fans out
at most64 destinations/event and executes one8-transition engine tick. It refuses
overlapping quanta without an unbounded waiter list. Work/control have separate
2-slot HTTP budgets, and finite stop/revoke/run-cancel routes use existing
control admission and remain available through maintenance. An individual
synchronous call remains uninterruptible; all configured SQL and caller deadlines
are separate from transition counts. Events are accepted/frozen before fanout;
blocked fanout remains retained. A corrupt/missing-key outbox currently holds its
driver quantum for owner recovery; no quarantine/GC/capacity claim is made.

Migration0023 is a candidate table definition only at this checkpoint: native0022
is reserved and not yet integrated here. Component fixtures explicitly install
candidate tables in disposable databases while leaving the registered meta
version unchanged. No ordered migration-gap acceptance or production startup
qualification is claimed. Root must register021/022/023/024 in actual order and
qualify that final union separately.

Review source boundaries:

- host actor: exact registry, acquired-owner callback, keyed admission, explicit
  recovery/cancel modes, bounded event/fanout/public inspection;
- playbooks/domain: fixed command/event schemas, typed recipient/grant identity;
- playbooks/service: namespace/grant/current-role locks, protected inbox/outbox,
  atomic existing-mailbox effect, retained replay/cancel/ACK;
- playbooks/driver: acquired-engine/current-runtime adoption, immutable replay,
  bounded manual quantum; no automatic replacement/mutation retry;
- playbooks/api + optional builder: owner/CSRF/maintenance layers, independent
  work/control budgets, no worker routes or generic engine mutation proxy;
- public registry/fixtures: generated from the sole Rust registry source.

Tests cover actual protected engine+AQ mailbox/event consumption, deliberately
omitted AQ admission/event ACKs, owner restart, expired/revoked grants, changed
identity bytes, protected data, quota rollback, immutable task/session scope,
maintenance and role/CSRF boundaries. Real listeners prove execution renewal and
cancellation continue while a real mailbox Host lock hold exhausts both bridge
work permits. Synthetic known task bindings qualify mailbox/authority boundaries;
they do not qualify a native harness, provider or live Discord transport.

Next capability profiles must preserve profile1 bytes: decision/check/review
owners need explicit new catalog/profile admission, revision-bound subject and
human eligibility. A model output or notification delivery is not human approval;
a posted passed:true is not executed check evidence. AQ14's ledger/adapters must
coordinate with the owning result evaluator rather than duplicate it. Full
hibernation/continuation and AQ27 wait/GATE05 remain outside this checkpoint.

## Exact clean component qualification

Source3503897232092f02a2ae93e54a0a8794548db1c2 passed94 selected daemon
regression executions (including11 bridge PG/API and2 contract cases),8 host
component tests and1 explicit protected-PG host test. Both strict lint profiles,
formatting,477-file source drift and clean checkout status passed.
[Machine evidence](AQ-13-first-bridge.json) pins commands/hashes and
[retained logs](evidence/aq13-bridge-3503897).

The initial default-parallelism regression attempt failed10 execution fixture
initializations with PostgreSQL SQLSTATE53200/out-of-shared-memory, suggesting
max_locks_per_transaction during concurrent per-test migrations. Its failed log
is retained, not counted as a code pass. The unchanged exact source then passed
all94 selected executions with explicit --test-threads=2. No database server
configuration or source assertions were relaxed. Separate internal concurrency
and actual HTTP saturation tests still execute their intended races.

Test-only supplement8fd3b0f separates candidate mode from later ordered
integration: if the compiled state version is>=23, the fixture requires the
normal migration to have created bridge tables; it never repairs them. Earlier
candidate mode explicitly applies the table definition and asserts meta version
did not change. Normal ordered migration qualification belongs to root's final
022/023/024 source union and is not inferred from these component fixtures.
