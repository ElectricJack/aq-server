# GATE04 real Graph fixture component qualification

Date: 2026-11-01. Independently reviewed source
`5fec17cbd17c3d7b0b583c95940cfed3df65230f`, based on agent-q `1db85bb`; approved
[plan](../../GATE-04-real-graph-fixture-plan.md) at `a18c338`, timing-limit note
`e92a8c2`. This is an **actual Graph component fixture**, not a Codex task cycle,
AQ completion claim, review, B1/B2/C settlement or production Graph rollout.

[qualification.json](qualification.json) records exact source/binary hashes,
counts, actual IDs/receipts and cleanup. [actual-receipts.json](actual-receipts.json)
contains the actual Graph mutation receipts and read-only owner evidence. No
provider credentials, Graph bearer secrets or DSNs are retained.

| Check | Result |
| --- | --- |
| Offline/locked compile, jobs 2, debug 0, incremental 0 |PASS, 68 s |
| Pure bounded listening-log parser |1 passed; 0 failed/ignored; 1 actual case filtered |
| Focused Clippy, warnings denied |PASS, 42.82 s |
| One explicitly selected actual process/HTTP/SQL smoke |1 passed; 0 failed/ignored; 1 pure case filtered; 0.86 s |

The actual test is opt-in in the ordinary suite. Qualification selected it with
`--ignored --exact actual_pinned_graph_fixture_cas_receipts_context_and_owned_cleanup`;
it ran and passed rather than being silently skipped. There was exactly one Graph
service launch and no failed service trial or changed retry. Compile/strict passed
before that launch. Target2 was then released; no fixture process remains.

## Actual owner facts

The exact existing Graph package0.1.0 executable SHA256
`eb9c8f41002cb5d9675f87de0cadbe34e7d74cfc60d2449cd23ac4df76921fea`
was copied privately, rehashed and served on its kernel-selected numeric-loopback
port. It migrated a fresh TestDb-owned UUID database to its actual schema8, performed
its real bootstrap and granted a distinct writer through the public API. The helper
never called AQ Store migration or directly seeded a Graph domain/receipt row.
Actual SQL verified the writer actor/capability/token verifier. Service tokens stayed
in private0600 files outside any worker/harness; no worker or provider was launched.

- Workspace: `e33104ef-cc73-4bf9-9db2-a1804e38db8c`.
- Task: `e2370e57-11d5-4e96-9fc9-fa64a7812302`.
- Creation change set: `2f96c9ff-d45c-478a-bdf2-fc91c827764a`.
- Completion change set: `a7494506-f2f9-4b33-b02b-2be7d1d07494`.
- Actual CAS/logical revisions:1→2. Initial task was active; final task completed.
- Exactly one node and zero edges; initial actual context was complete with no
  neighbors/dependencies/pinned neighboring revisions. No context was trimmed.
- Identical creation and completion requests replayed exact original receipts.
  A fresh operation with stale expected version1 returned409 without overwriting2.
  Read-back equalled the actual completion result. Writer receipts grew1→2.

`before.task` and `after.task` deliberately retain the **original immutable seed
pin** from `task_pin()`; `after.task` is not a new observation of current version1.
The actual current version2 is in the completion receipt and matched the real
post-mutation task read. `startup_log_sha256` hashes the bounded process log at the
read time and may change as ordinary safe metadata logs append; it is not the
executable/source identity hash.

The owned child PID1000405 received SIGTERM, exited successfully and was actually
reaped; no kill fallback was needed. Only then were private evidence connections
closed and the exact UUID database dropped. The helper's real admin query confirmed
that database's absence, then TempDir.close confirmed private-directory removal.
A separate read-only postcheck also found the child absent in `/proc`. Database/
directory names stay fixture-internal; the reviewed helper records the actual
ordered checks rather than inventing public resource-name evidence afterwards.
The original canonical4195 process/binary/database and user-dirty main documents
were not changed. No container, dependency, Git, provider, worker or AQ task owner
was started by this smoke.

The existing TestDb Drop uses a blocking thread.join and pool.close awaits cleanup.
This run measured successful cleanup but does not prove a universal total database
teardown deadline. Child polling and the later absence check are individually
bounded. Unconfirmed child exit/DB cleanup preserves exact owned resources and
uncertainty; it never reports successful cleanup.

## Runner handoff and scope

Reuse `tests/support/real_graph.rs`; source consumers can import that file without
changing the production application or the synthetic fixtures used by other tests.
Call `RealGraphFixture::start(BinaryPin, TaskSeed)` before any AQ project verification,
resolver or Poll, and install its `agent_access_config()` on the outer AQ owner.
The caller creates the actual AQ project and binds the helper's fresh workspace/task
IDs and actual starting pin. Keep the fixture alive through real owner settlement;
then call explicit `shutdown()` and retain its cleanup evidence. Do not reuse this
already-completed, destroyed smoke task for a Codex run.

The first pilot admits mutable Graph set **{task}**, with no required neighboring
context; immutable catalog/source artifacts are separate pins. The initial context
and B2 checks must honestly establish that scope. Task CAS does not atomically
assert absence of every arbitrary incoming nonstructural citation. A policy requiring
that stronger condition or broader mutable inputs must refuse until a separate
Graph transaction-assertion owner exists. No no-op neighboring writes or broader
Graph guard/rollout were introduced here. B1/B2/C, authenticated completion and the
actual Codex/Git/tool cycle remain their owners' work.
