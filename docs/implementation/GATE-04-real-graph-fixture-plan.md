# GATE04 disposable actual Graph owner fixture

Status: source plan only, based on agent-q `1db85bb` and aq-graph `225329c`.
No fixture database/service or provider call has been started. Root must review
these exact pins, ownership and cleanup before execution. Existing canonical
Graph4195 and its database, AQ canonical state and main user documents stay intact.

## Smallest reusable seam

Add test-only `crates/aq-server/tests/support/real_graph.rs`, shared by a focused
opt-in process/SQL test and Runner's actual Codex composition. No production route,
Graph rollout, new Rust dependency, container or mandatory external service is added.
Reuse the existing test PostgreSQL55432 cluster and exact AQ `TestDb` UUID create/
Drop guard; do not call `TestDb::store()` for a Graph database. Only the actual
Graph binary runs its own embedded migrations and public bootstrap/API operations.

Proposed API:

```rust
pub struct BinaryPin { pub path: PathBuf, pub sha256: String }
pub struct TaskSeed {
    pub workspace_id: Uuid, pub task_id: Uuid,
    pub title: String, pub body: String, pub acceptance_criteria: String,
}
pub struct GraphTaskPin {
    pub workspace_id: Uuid, pub task_id: Uuid,
    pub version: i64, pub content_revision: i64,
}
pub struct RealGraphFixture { /* owns child, TestDb, private files/client */ }
impl RealGraphFixture {
    pub async fn start(pin: BinaryPin, seed: TaskSeed) -> anyhow::Result<Self>;
    pub fn origin(&self) -> &str;
    pub fn task_pin(&self) -> &GraphTaskPin;
    pub fn creation_receipt(&self) -> &serde_json::Value;
    pub fn agent_access_config(&self) -> AgentAccessConfig;
    pub async fn assert_task_only_context(&self) -> anyhow::Result<()>;
    pub async fn owner_evidence(&self) -> anyhow::Result<serde_json::Value>;
    pub async fn shutdown(self) -> anyhow::Result<serde_json::Value>;
}
```

The owning fixture retains all private credentials; no Debug/serialization exposes
secrets. `AgentAccessConfig` points only to its private service-token file and
records the actual authenticated writer actor. Runner inserts that configuration
**before any project verification/resolver/Poll**. Replacing the synthetic peer
only after an earlier Verified observation is not a real-owner proof. No ambient
environment variable mutation is used to switch an already-running fake fixture.
Runner owns AQ project creation, fresh project UUID, catalog/source bindings and
configuration integration. Graph owns its workspace/task/receipt only. The Graph
helper does not fabricate a Verified AQ project or create grants/worker inventory.

## Exact existing binary and private process

Pinned existing executable:

- path `/home/jkern/dev/aq-graph/target/release/aq-graph`;
- SHA256 `eb9c8f41002cb5d9675f87de0cadbe34e7d74cfc60d2449cd23ac4df76921fea`;
- package0.1.0, source225329c, Graph migrations1..8;
- additive committed-observation schema0009/profile is intentionally absent.

At fixture start, validate a regular executable and exact bytes, then copy and
rehash it inside one private0700 temporary directory before execution. This avoids
rebuild/path substitution between check and exec and never modifies the original.
Create original random admin/service credentials once in0600 non-symlink files;
keys are fixture Graph credentials, not provider credentials. Use create_new and
private owner-only paths. No secret values/DSNs enter retained evidence or argv.

Launch the owned copy with explicit `serve --bind 127.0.0.1:0 --pool-size 4`, fresh
bootstrap workspace UUID/actor and token FILE. Clear inherited environment and
supply only required locale/log settings, private HOME and this TestDb's database
URL. Stdin is null; stdout/stderr are private0600 files. The binary's actual
`aq-graph listening address=127.0.0.1:<port>` line supplies the kernel-selected
endpoint; bound and strip ANSI log formatting, reject absent/ambiguous/nonloopback/
zero addresses. No close-then-rebind port guess, fixed port or proxy is used.
Startup/log polling is bounded (15s/64KiB), checks owned child exit, and then probes
actual `/health/ready` with API-version1. Failures clean up; they never fall back to
canonical4195 or another service. HTTP uses direct numeric-loopback origin,
no_proxy, no redirects, bounded connection/request/body and explicit API header.

## Actual workspace, task, authority and receipts

1. `TestDb::create()` creates a fresh random UUID database on the test admin
   database. Explicit guard requires host127.0.0.1, port55432, database/user postgres;
   only its options/UUID name enter the fixture. No existing database is selected.
2. The child performs the actual Graph migrations and bootstrap. `/health/ready`
   verifies the compiled migration set. Read-only SQL may independently confirm
   schema1..8 and this exact workspace/credential actor/capability/token-verifier
   ownership. No direct SQL graph seeding or synthetic HTTP peer is allowed.
3. Admin POST `/v1/workspaces/{workspace}/credentials` creates a distinct random
   writer identity with capabilitywrite. Admin secret remains fixture-private.
4. Writer POST `/mutations` creates exactly one explicitly active task from the
   agreed TaskSeed, using a stable idempotency UUID. Preserve the actual complete
   creation receipt/change_set_id and task CAS/content_revision from its response.
   Repeat the identical seed operation once and require the identical actual receipt.
5. Authenticated actual GET task/ready/context validates the task, workspace, actual
   version/revision and complete one-node/no-edge/no-dependency context. No truncated
   page or unexpected neighbor is silently removed to make this assertion pass.

Suggested first-cycle TaskSeed, subject to Runner/Work's exact completion tool:

- title: `Complete the isolated GATE04 workspace edit`;
- body: `Create gate04-proof.txt containing exactly gate04-complete followed by a newline. Submit the completion claim through the configured authenticated AQ command. Do not modify Graph directly or edit other files.`;
- acceptance criteria: `The retained source commit contains exactly the approved file edit. Actual result, review, effect-disposition and settlement owners confirm their receipts before the task is completed.`

Runner may freeze a different explicit task text before launch; helper never
manufactures task content during a later retry. Fresh workspace/task/project UUIDs
are allocated before actual project verification and retained through the case.
No Graph token, database socket or Graph origin enters the harness. The outer AQ
owner accesses Graph normally; Runner's existing FD transfer exposes only the AQ
worker router inside the private worker view.

## Completion fence and deliberately restricted first pilot

The complete admitted mutable Graph input set is **exactly this task**. Its initial
CAS version and logical content_revision join the original execution spec, source
commit/artifact, report/stop, review and effect-disposition receipts under B2/C.
The context must be proven complete with zero relationships/dependencies/other
mutable nodes; immutable catalog/source artifacts are separate admitted pins.
Broader tasks are refused, not trimmed or rebound.

C uses the real Graph `/mutations` transaction: one `update_node` for the original
task with `expected_version=ORIGINAL_ADMITTED_VERSION`, patchtask_state=completed,
and the stable completion idempotency key. Exact HTTP success receipt/change_set_id
and resulting task version/logical revision join the durable AQ intent/outbox.
A changed task conflicts, never retries with a newer version. Read-back is useful
inspection, not a substitute for the owning idempotent mutation receipt. B1/B2/C
and actual task settlement remain Decisions/Results ownership; the fixture must
not perform pilot completion ahead of their reviewed predicates.

Source audit: existing Mutation enum (`aq-graph/src/domain.rs:275..373`) has only
write operations and target `expected_version`; MutationRequest has no independent
transaction-level input read assertions. `UpdateNode` locks/checks its target;
completion also checks current structural invariants, which do not establish
admitted equality of unrelated input revisions. Several preflight GETs followed by
one task CAS would be insufficient for a broader mutable input set. No-op writes to
assert unrelated nodes are excluded and must not bump or overwrite those inputs.

Deferred narrow extension, **not this pilot**: a new explicit guarded-mutation
profile could accept bounded exact node/edge read pins, lock/assert them together
with the task write in one owner transaction, bind the assertions into the stable
idempotency request and return typed conflict without any effect when invalidated.
It needs all supported writer coverage, row ordering/serialization tests and a
separate compatible deployment. No such Graph code/rollout is authorized here.

## Cleanup, source checks and one qualified smoke

`RealGraphFixture` owns only its exact Child handle, temp directory and TestDb.
Explicit shutdown checks/terminates that child with SIGTERM, bounded wait3s, then
only its same unreaped child receives kill fallback and wait. Reap child before
closing private files/pools and dropping its UUID database; assert exact database
absence through the admin connection. Drop supplies the same owned-child safety
net for panic/setup failure. No PID name search, service restart, directory glob,
container stop or canonical cleanup exists. Startup failures preserve bounded
sanitized stage/status evidence while deleting only owned resources.

Before execution: review this plan/pin and helper source; coordinate Runner's
SERVER and cold-Git tests. Compile focused helper/consumer checks under a target
lease. Then one explicit opt-in smoke can exercise real health/auth/task creation,
identical idempotency replay, correct expected-version completion on its own smoke
task, stale-version409, receipt verification and cleanup. That smoke is Graph-only,
not GATE04 model execution or B2/C success. The actual Codex cycle later creates its
own untouched task and admits only its genuine owner observations.
