# Sealed history phase A (live-only caps, offer rate limit, visible holds) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every history cap on the native swarm path counts only live rows. An offer rate limit replaces the lifetime bound. A refused offer is a visible hold in `GET /api/v1/swarm`, never a 409 loop. Nothing is deleted.

**Architecture:**
- Migration 0103 adds SQL functions that are the only definition of a live row (`sealed_attempt_live`, `sealed_history_usage`, `sealed_staging_live_bytes`), the limits table (`sealed_history_limits`) and the hold check (`sealed_history_holds`).
- Migration 0104 makes five existing quota triggers call `sealed_history_usage`.
- Migration 0105 adds a database fence on the offer rate.
- A new crate, `aq-history-caps`, mirrors the Rust-side checks, with a fallback to the old all-rows queries before 0103. It runs the poll gate and builds the swarm holds.
- aq-swarm maps the holds to two new `HoldCode` values.
- Moving the pure fair-share ordering into `aq-profiled-state` pays for the lines the aq-server lib gains.

**Tech Stack:** Rust 2024, sqlx 0.8 on PostgreSQL (PL/pgSQL), schemars 1, nextest; aq-dashboard TypeScript for the re-pin.

**Spec:** `docs/specs/0008-sealed-history-retention.md` §5 (inventory), §6 (live), §7 (phase A), §11 (acceptance).

## Global Constraints

- **Repositories and branches:**
  - aq-server branch `sealed-history-pa/server`, from `origin/main`;
  - aq-dashboard branch `sealed-history-pa/dashboard`, from `origin/main`;
  - one PR per repo; the two merge together (Task M1);
  - never use `git stash`; never edit `/home/jkern/dev/agent-queue2`.
- **Migrations:**
  - the current maximum on main is `0102`, so this plan adds `0103`, `0104` and `0105`, contiguous;
  - each file ends with `UPDATE meta SET value='N' WHERE key='state_format_version';` (N = 103, 104, 105);
  - if main gains a migration before this merges, renumber to the next free numbers and change the `meta` lines and every reference to the numbers in this plan's tests;
  - the build script discovers migrations from the directory (`crates/aq-server/migration_catalog.rs`); no other registration is needed.
- **aq-server build rules (AGENTS.md):**
  - integration tests go only into the existing 8 binaries; no new `[[test]]` target and no new top-level `tests/*.rs` file;
  - `build-budget.toml` values only go down;
  - the aq-server lib is at **109,185 of a 109,185 ratchet**, so every line added to `crates/aq-server/src` must be paid for in this PR. Task 1 pays about 100 lines; Task 9 checks the result;
  - new code goes into the new crate `crates/aq-history-caps`;
  - every `cargo test`/`cargo nextest` runs under `timeout 900`;
  - this worktree builds into its own `./target`;
  - a test that builds a selection fixture must be in the nextest `selection-fixture` group (`.config/nextest.toml`). `native_offers::*` already is, through its module regex. `schema33_phase_terminal::*` tests are listed by exact name, so a new one must be added.
- **Never change the bodies of other migrations.** Migration 0104 redefines functions with `CREATE OR REPLACE`. It keeps every function name, every trigger, and every check except the counted quota lines.
- **Contracts:**
  - `protocol/dashboard-read/v1/schema.json` is generated: `cargo run -p aq-server --example dashboard_contract`, checked with `-- --check`;
  - a server change to `dashboard-read` merges together with the aq-dashboard re-pin (owner rule, retrospective W76);
  - no worker wire contract changes (aq-admission compatibility files stay byte-identical).
- **Copy:** the hold messages and remedies are exactly as written in Task 4.
- **No time estimates** anywhere: in code comments, PR text or reports.
- **Commits** end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. PR descriptions end with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- **Test database:** `scripts/test-db` prints the `DATABASE_URL` (see `docs/development.md`). Every test command below assumes it is exported.

## Review Focus

1. **A finished run that still holds its task is live.** `sealed_attempt_live` must return true for `holds_task = true` whatever the status, and false only when the task is released **and** the status is `finished`, `declined` or `never_granted`. Pinned by `sealed_history_live::attempt_live_follows_the_hold_and_the_status`.
2. **A shared admission artifact** linked by a settled run and a live run is live. Pinned by `sealed_history_live::artifact_usage_counts_live_links_and_pending_rows_only` (artifact `a1` is linked by both).
3. **The gate refuses before writing.**
   - A held poll must return 200 with no offers and write no decision, artifact or export row. Pinned by `native_offers::history_caps::offer_rate_limit_holds_the_node_without_a_409`, which counts `intelligence_decisions` and `admission_artifacts` before and after.
   - Headroom (64 artifacts) must make the gate hold before the trigger would refuse. Pinned by `sealed_history_live::holds_report_the_offer_rate_and_the_project_artifact_cap`.
4. **Older state formats keep working.** Fixtures at format 32 (`with_state32_store`) issue grants before 0103 exists. `aq_history_caps::usage` must fall back to the old all-rows queries, and `live_usage` must return `None`. Pinned by the existing `it_schema schema33_phase_terminal::*` and `it_native native_initial_grant::*` tests staying green.
5. **The 409 loop of 2026-11-07 cannot recur silently.** Holds appear on the node in `/api/v1/swarm`. Pinned by `dashboard_swarm::swarm_shows_the_offer_rate_hold_on_the_node` and the aq-swarm unit test `history_holds_join_the_node_and_the_waiting_reasons`.

---

## File structure

| File | Change |
| --- | --- |
| `Cargo.toml` (workspace) | add `"crates/aq-history-caps"` to `members` |
| `crates/aq-history-caps/Cargo.toml`, `src/lib.rs` | new crate (Task 4) |
| `crates/aq-profiled-state/src/fairness.rs`, `src/lib.rs` | moved fair-share ordering (Task 1) |
| `crates/aq-server/src/profiled_execution/producer/fairness.rs` | re-export the moved items (Task 1) |
| `crates/aq-server/migrations/0103_sealed_history_live.sql` | new (Task 2) |
| `crates/aq-server/migrations/0104_sealed_history_live_caps.sql` | new (Task 3) |
| `crates/aq-server/migrations/0105_sealed_offer_rate.sql` | new (Task 6) |
| `crates/aq-server/Cargo.toml` | add `aq-history-caps` (Task 5) |
| `crates/aq-server/src/intelligence/resolver.rs` | decisions mirror (Task 5) |
| `crates/aq-server/src/profiled_execution/closure/history.rs` | registrations mirror (Task 5) |
| `crates/aq-server/src/profiled_execution/source_exports.rs` | exports and staging mirror (Task 5) |
| `crates/aq-server/src/source_retention.rs` | staging mirror, two sites (Task 5) |
| `crates/aq-server/src/profiled_execution/preparation.rs` | preparation usage (Task 5) |
| `crates/aq-server/src/profiled_execution/accepted_preparation.rs` | v2 proposal count (Task 5) |
| `crates/aq-server/src/profiled_execution/producer/workflow.rs` | the poll gate (Task 7) |
| `crates/aq-dashboard-contract/src/swarm_contract.rs` | two `HoldCode` variants (Task 8) |
| `crates/aq-swarm/Cargo.toml`, `src/assemble.rs`, `src/tests.rs` | history holds in the swarm (Task 8) |
| `crates/aq-server/src/dashboard_api/swarm.rs` | load the holds (Task 8) |
| `protocol/dashboard-read/v1/schema.json`, `README.md` | regenerated schema, README text (Task 8) |
| `crates/aq-server/tests/common/mod.rs`, `tests/common/sealed.rs` | seeding helpers (Task 2) |
| `crates/aq-server/tests/it_schema/main.rs`, `sealed_history_live.rs` | new module (Tasks 2, 3, 6) |
| `crates/aq-server/tests/it_schema/schema33_phase_terminal.rs` | one test updated, one added (Task 3) |
| `crates/aq-server/tests/it_profiled/profiled_artifacts.rs` | one test added (Task 3) |
| `crates/aq-server/tests/it_native/native_offers.rs`, `native_offers/history_caps.rs` | two tests (Task 7) |
| `crates/aq-server/tests/it_dashboard/dashboard_swarm.rs` | one test (Task 8) |
| `.config/nextest.toml` | one exact name added to `selection-fixture` (Task 3) |
| `docs/implementation/0008-PA-server.md` | what was built (Task 9) |
| aq-dashboard `src/api/wire/swarm.ts`, `contracts/pinned/dashboard-read/v1/*` | re-pin (Task D1) |

---

## aq-server tasks

### Task 1: Move the pure fair-share ordering into `aq-profiled-state` (pays the line budget)

**Files:**
- Create: `crates/aq-profiled-state/src/fairness.rs`
- Modify: `crates/aq-profiled-state/src/lib.rs`, `crates/aq-server/src/profiled_execution/producer/fairness.rs`

- [ ] **Step 1: Create `crates/aq-profiled-state/src/fairness.rs`.** Its first lines:

```rust
//! Fair multi-project scheduling (worker spec §8.2): configured priority plus
//! aging, with a hard starvation bound. Pure ordering; the durable last-service
//! record stays with its store owner in aq-server (`producer::fairness`).
use std::collections::BTreeMap;
use uuid::Uuid;
```

  Then **move**, verbatim, from `crates/aq-server/src/profiled_execution/producer/fairness.rs`:
  - the doc comments and items `STARVATION_MS`, `AGING_MS`, `Candidate` and `order` (current lines 8-48);
  - the synchronous test `two_projects_share_capacity_fairly` (the `#[test]` at current lines 91-149). Put it under `#[cfg(test)] mod tests { use super::*; ... }`.

- [ ] **Step 2:** In `crates/aq-profiled-state/src/lib.rs`, add `pub mod fairness;` next to the other `pub mod` lines.

- [ ] **Step 3:** In `producer/fairness.rs`:
  - delete the moved items and the moved test;
  - keep `SCHEDULE_FORMAT`, `recorded`, `served`, `record`, and the `#[tokio::test] service_record_never_moves_backwards` test;
  - after the `use` lines, add:

```rust
pub use aq_profiled_state::fairness::{AGING_MS, Candidate, STARVATION_MS, order};
```

  Keep `use std::collections::BTreeMap;` (`served` uses it) and `use uuid::Uuid;`. Delete any other `use` the compiler reports unused.

- [ ] **Step 4: Verify.**

```sh
timeout 900 cargo test -p aq-profiled-state fairness
```
Expected: `test fairness::tests::two_projects_share_capacity_fairly ... ok`.

```sh
timeout 900 cargo nextest run -p aq-server --lib -E 'test(/fairness/)'
python3 scripts/build-budget.py
```
Expected: `service_record_never_moves_backwards` passes. The budget prints an aq-server lib count about 100 below 109185 and `OK`. Record the number in the commit message.

- [ ] **Step 5: Commit** `refactor(scheduling): move the pure fair-share ordering into aq-profiled-state` (with the line count).

### Task 2: Migration 0103, the live definition and its tests

**Files:**
- Create: `crates/aq-server/migrations/0103_sealed_history_live.sql`, `crates/aq-server/tests/common/sealed.rs`, `crates/aq-server/tests/it_schema/sealed_history_live.rs`
- Modify: `crates/aq-server/tests/common/mod.rs`, `crates/aq-server/tests/it_schema/main.rs`

- [ ] **Step 1: Write `0103_sealed_history_live.sql` exactly:**

```sql
-- Spec 0008 phase A: a sealed history row counts toward a cap only while it is
-- live: its run is live, or nothing references it yet and it was written within
-- 10 minutes (pending). One definition for the triggers, the Rust mirror checks,
-- the poll gate and the swarm read. Nothing is deleted.
CREATE FUNCTION sealed_attempt_live(p_attempt UUID) RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h ON h.attempt_id=e.attempt_id
  WHERE e.attempt_id=p_attempt
  AND (e.holds_task OR COALESCE(h.state->>'status',e.state->>'status','unknown') NOT IN ('finished','declined','never_granted')))
$$;
CREATE FUNCTION sealed_history_limits()
RETURNS TABLE(kind TEXT, scope TEXT, max_rows BIGINT, max_bytes BIGINT, headroom_rows BIGINT, headroom_bytes BIGINT)
LANGUAGE sql IMMUTABLE AS $$
 SELECT v.k,v.s,v.r::BIGINT,v.b::BIGINT,v.hr::BIGINT,v.hb::BIGINT FROM (VALUES
  ('admission_artifacts','global',4096,268435456,64,4194304),
  ('admission_artifacts','project',512,67108864,64,4194304),
  ('intelligence_decisions','global',1024,67108864,2,16777216),
  ('launch_registrations','global',4096,262144,1,64),
  ('launch_registrations','node',256,16384,1,64),
  ('native_preparation','global',4096,67108864,1,262144),
  ('native_preparation','node',256,4194304,1,262144),
  ('offers','global',960,NULL,1,0),
  ('offers','node',30,NULL,1,0),
  ('phase_terminal','global',256,268435456,1,1048576),
  ('phase_terminal','node',16,16777216,1,1048576),
  ('presence_operations','global',65536,268435456,1,0),
  ('presence_operations','node',4096,16777216,1,0),
  ('presence_rate','global',11520,NULL,1,0),
  ('presence_rate','node',720,NULL,1,0),
  ('project_offers','project',240,NULL,1,0),
  ('source_exports','global',4096,268435456,1,4202496),
  ('source_exports','project',256,67108864,1,4202496),
  ('v2_proposals','global',4096,NULL,1,0),
  ('v2_proposals','node',256,NULL,1,0)
 ) v(k,s,r,b,hr,hb)
$$;
CREATE FUNCTION sealed_history_usage(p_kind TEXT, p_scope UUID, p_now_ms BIGINT, p_exclude UUID DEFAULT NULL)
RETURNS TABLE(total_rows BIGINT, total_bytes BIGINT, scope_rows BIGINT, scope_bytes BIGINT)
LANGUAGE plpgsql STABLE AS $$
BEGIN
 IF p_kind='admission_artifacts' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(octet_length(a.body)),0)::BIGINT,
   (count(*) FILTER(WHERE a.owner_key=p_scope::TEXT))::BIGINT,
   COALESCE(sum(octet_length(a.body)) FILTER(WHERE a.owner_key=p_scope::TEXT),0)::BIGINT
  FROM admission_artifacts a
  WHERE EXISTS(SELECT 1 FROM profiled_launch_artifacts l WHERE l.artifact_id=a.artifact_id AND sealed_attempt_live(l.attempt_id))
   OR (a.created_at_ms>=p_now_ms-600000 AND NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts l WHERE l.artifact_id=a.artifact_id));
 ELSIF p_kind='intelligence_decisions' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(pg_column_size(d.decision)),0)::BIGINT,
   count(*)::BIGINT,COALESCE(sum(pg_column_size(d.decision)),0)::BIGINT
  FROM intelligence_decisions d
  WHERE EXISTS(SELECT 1 FROM profiled_launches l WHERE l.decision_id=d.decision_id AND sealed_attempt_live(l.attempt_id))
   OR (COALESCE((d.decision#>>'{inputs,now_ms}')::BIGINT,0)>=p_now_ms-600000
       AND NOT EXISTS(SELECT 1 FROM profiled_launches l WHERE l.decision_id=d.decision_id));
 ELSIF p_kind IN ('source_exports','staging_exports') THEN
  RETURN QUERY SELECT count(*)::BIGINT,
   COALESCE(sum(CASE WHEN p_kind='staging_exports' THEN e.charged_bytes
     ELSE octet_length(e.request_bytes)+COALESCE(octet_length(e.verified_bytes),4194304) END),0)::BIGINT,
   (count(*) FILTER(WHERE e.project_id=p_scope))::BIGINT,
   COALESCE(sum(CASE WHEN p_kind='staging_exports' THEN e.charged_bytes
     ELSE octet_length(e.request_bytes)+COALESCE(octet_length(e.verified_bytes),4194304) END) FILTER(WHERE e.project_id=p_scope),0)::BIGINT
  FROM profiled_source_exports e
  WHERE e.operation_id IS DISTINCT FROM p_exclude
   AND (e.state='pending'
    OR EXISTS(SELECT 1 FROM profiled_launch_source_exports l WHERE l.operation_id=e.operation_id AND sealed_attempt_live(l.attempt_id))
    OR EXISTS(SELECT 1 FROM source_bases b WHERE b.profiled_export_operation_id=e.operation_id AND sealed_attempt_live(b.attempt_id)));
 ELSIF p_kind='native_preparation' THEN
  RETURN QUERY WITH leases AS (
    SELECT m.lease_id,m.node_id,octet_length(m.lease::text)::BIGINT AS bytes FROM admission_material_leases m
    WHERE m.state='pending' OR sealed_attempt_live(COALESCE(m.admitted_attempt_id,m.prospective_attempt_id))),
   charged AS (
    SELECT l.node_id,l.bytes FROM leases l
    UNION ALL SELECT r.node_id,octet_length(r.response::text)::BIGINT FROM profiled_execution_receipts r
    UNION ALL SELECT l.node_id,octet_length(mr.receipt::text)::BIGINT FROM admission_material_receipts mr JOIN leases l ON l.lease_id=mr.lease_id
    UNION ALL SELECT p.node_id,(octet_length(p.proposal::text)+octet_length(p.policy::text))::BIGINT FROM profiled_v2_proposals p WHERE sealed_attempt_live(p.attempt_id)
    UNION ALL SELECT a.node_id,(octet_length(a.request::text)+octet_length(a.receipt::text))::BIGINT FROM profiled_v2_accepts a WHERE sealed_attempt_live(a.attempt_id)
    UNION ALL SELECT g.node_id,octet_length(g.request_bytes)::BIGINT+octet_length(g.receipt_bytes)+octet_length(g.provenance_bytes)
     FROM profiled_phase_execution_grants g WHERE sealed_attempt_live(g.attempt_id))
  SELECT (SELECT count(*) FROM leases)::BIGINT,(SELECT COALESCE(sum(c.bytes),0) FROM charged c)::BIGINT,
   (SELECT count(*) FROM leases l WHERE l.node_id=p_scope)::BIGINT,(SELECT COALESCE(sum(c.bytes),0) FROM charged c WHERE c.node_id=p_scope)::BIGINT;
 ELSIF p_kind='v2_proposals' THEN
  RETURN QUERY SELECT count(*)::BIGINT,0::BIGINT,(count(*) FILTER(WHERE p.node_id=p_scope))::BIGINT,0::BIGINT
  FROM profiled_v2_proposals p WHERE sealed_attempt_live(p.attempt_id);
 ELSIF p_kind='launch_registrations' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(r.logical_bytes),0)::BIGINT,
   (count(*) FILTER(WHERE r.node_id=p_scope))::BIGINT,COALESCE(sum(r.logical_bytes) FILTER(WHERE r.node_id=p_scope),0)::BIGINT
  FROM profiled_launch_registrations r WHERE sealed_attempt_live(r.attempt_id);
 ELSIF p_kind='phase_terminal' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(c.reserved_bytes),0)::BIGINT,
   (count(*) FILTER(WHERE c.node_id=p_scope))::BIGINT,COALESCE(sum(c.reserved_bytes) FILTER(WHERE c.node_id=p_scope),0)::BIGINT
  FROM profiled_phase_terminal_capacity c WHERE sealed_attempt_live(c.attempt_id);
 ELSIF p_kind='presence_operations' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(octet_length(o.request_bytes)+octet_length(o.receipt_bytes)+1024),0)::BIGINT,
   (count(*) FILTER(WHERE o.node_id=p_scope))::BIGINT,
   COALESCE(sum(octet_length(o.request_bytes)+octet_length(o.receipt_bytes)+1024) FILTER(WHERE o.node_id=p_scope),0)::BIGINT
  FROM profiled_presence_operations o
  WHERE o.expires_at_ms>p_now_ms OR (o.kind='presence' AND sealed_attempt_live(o.attempt_id));
 ELSIF p_kind='presence_rate' THEN
  RETURN QUERY SELECT count(*)::BIGINT,0::BIGINT,(count(*) FILTER(WHERE o.node_id=p_scope))::BIGINT,0::BIGINT
  FROM profiled_presence_operations o WHERE o.recorded_at_ms>p_now_ms-3600000;
 ELSIF p_kind IN ('offers','project_offers') THEN
  RETURN QUERY SELECT count(*)::BIGINT,0::BIGINT,
   (count(*) FILTER(WHERE (p_kind='offers' AND l.launch_binding#>>'{session,node_id}'=p_scope::TEXT)
                       OR (p_kind='project_offers' AND o.project_id=p_scope)))::BIGINT,0::BIGINT
  FROM profiled_launches l JOIN admission_source_observations o ON o.observation_id=l.source_observation_id
  WHERE o.observed_at_ms>p_now_ms-3600000;
 ELSE
  RAISE EXCEPTION 'unknown sealed history kind %',p_kind;
 END IF;
END $$;
CREATE FUNCTION sealed_staging_live_bytes() RETURNS BIGINT LANGUAGE sql STABLE AS $$
 SELECT (COALESCE((SELECT sum((t.manifest->>'manifest_bytes')::BIGINT+(t.manifest->>'bundle_bytes')::BIGINT) FROM source_transfers t WHERE NOT t.staging_released),0)
  +COALESCE((SELECT sum(b.reserved_bytes) FROM source_bases b),0)
  +(SELECT u.total_bytes FROM sealed_history_usage('staging_exports',NULL,0) u))::BIGINT
$$;
-- Node call (p_project NULL): node and server limits. Project call: project limits.
CREATE FUNCTION sealed_history_holds(p_node UUID, p_project UUID, p_now_ms BIGINT)
RETURNS TABLE(hold_code TEXT, hold_kind TEXT, hold_scope TEXT, hold_unit TEXT, hold_used BIGINT, hold_limit BIGINT)
LANGUAGE plpgsql STABLE AS $$
DECLARE l RECORD; u RECORD; r BIGINT; b BIGINT;
BEGIN
 FOR l IN SELECT * FROM sealed_history_limits() x ORDER BY x.kind,x.scope LOOP
  IF p_project IS NULL THEN
   CONTINUE WHEN l.scope='project' OR p_node IS NULL;
  ELSE
   CONTINUE WHEN l.scope<>'project';
  END IF;
  SELECT * INTO u FROM sealed_history_usage(l.kind,COALESCE(p_project,p_node),p_now_ms);
  r:=CASE WHEN l.scope='global' THEN u.total_rows ELSE u.scope_rows END;
  b:=CASE WHEN l.scope='global' THEN u.total_bytes ELSE u.scope_bytes END;
  hold_code:=CASE WHEN l.kind IN ('offers','project_offers','presence_rate') THEN 'history_rate_limited' ELSE 'history_cap_reached' END;
  hold_kind:=l.kind; hold_scope:=l.scope;
  IF r+l.headroom_rows>l.max_rows THEN
   hold_unit:='rows'; hold_used:=r; hold_limit:=l.max_rows; RETURN NEXT;
  ELSIF l.max_bytes IS NOT NULL AND b+l.headroom_bytes>l.max_bytes THEN
   hold_unit:='bytes'; hold_used:=b; hold_limit:=l.max_bytes; RETURN NEXT;
  END IF;
 END LOOP;
END $$;
CREATE INDEX sealed_launch_artifact_artifact ON profiled_launch_artifacts(artifact_id);
CREATE INDEX sealed_launch_decision ON profiled_launches(decision_id);
CREATE INDEX sealed_launch_source_observation ON profiled_launches(source_observation_id);
CREATE INDEX sealed_source_observation_time ON admission_source_observations(observed_at_ms);
CREATE INDEX sealed_material_lease_attempt ON admission_material_leases(admitted_attempt_id);
CREATE INDEX sealed_presence_operation_time ON profiled_presence_operations(node_id,recorded_at_ms);
CREATE INDEX sealed_source_base_export ON source_bases(profiled_export_operation_id) WHERE profiled_export_operation_id IS NOT NULL;
UPDATE meta SET value='103' WHERE key='state_format_version';
```

- [ ] **Step 2: Write `crates/aq-server/tests/common/sealed.rs`** (seeding helpers; every function runs inside a transaction the caller has put in `session_replication_role='replica'`, which disables triggers and FKs but not CHECK constraints):

```rust
//! Spec 0008 fixture seeding: relational-only sealed history rows written with
//! triggers and FKs disabled (`session_replication_role='replica'`). Never runnable
//! authority; only for counting tests.
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub async fn replica(pool: &PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

/// `status` None stores `{}`; Some("finished") stores `{"status":"finished"}`.
pub async fn attempt(c: &mut PgConnection, holds_task: bool, status: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),$2,CASE WHEN $3::text IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('status',$3::text) END)")
        .bind(id).bind(holds_task).bind(status).execute(&mut *c).await.unwrap();
    id
}

/// One offered launch: its source observation at `at_ms` and a launch binding on `node`.
pub async fn offer(c: &mut PgConnection, attempt: Uuid, decision: Uuid, node: Uuid, project: Uuid, at_ms: i64) {
    let observation = Uuid::new_v4();
    sqlx::query("INSERT INTO admission_source_observations(observation_id,project_id,source_policy_revision,observation,observed_at_ms,expires_at_ms) VALUES($1,$2,1,'{}'::jsonb,$3,$3+30000)")
        .bind(observation).bind(project).bind(at_ms).execute(&mut *c).await.unwrap();
    sqlx::query("INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) VALUES($1,$2,$3,jsonb_build_object('session',jsonb_build_object('node_id',$4::text)),'{}'::jsonb,repeat('0',64),'{}'::jsonb,repeat('0',64),'{}'::jsonb,'{}'::jsonb,$5+60000)")
        .bind(attempt).bind(decision).bind(observation).bind(node).bind(at_ms).execute(&mut *c).await.unwrap();
}

/// `n` offers on `node`/`project` at `at_ms`, with fresh attempt and decision ids.
pub async fn offers(pool: &PgPool, node: Uuid, project: Uuid, at_ms: i64, n: usize) {
    let mut tx = replica(pool).await;
    for _ in 0..n {
        offer(&mut tx, Uuid::new_v4(), Uuid::new_v4(), node, project, at_ms).await;
    }
    tx.commit().await.unwrap();
}

/// A one-byte (`x`) admission artifact of `project` written at `created_at_ms`.
pub async fn artifact(c: &mut PgConnection, project: Uuid, created_at_ms: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) VALUES($1,'{}'::jsonb,encode(sha256(convert_to($1::text,'UTF8')),'hex'),1,'graph_snapshot',$2,decode('78','hex'),$3,$3+86400000)")
        .bind(id).bind(project.to_string()).bind(created_at_ms).execute(&mut *c).await.unwrap();
    id
}

pub async fn link_artifact(c: &mut PgConnection, attempt: Uuid, artifact: Uuid) {
    sqlx::query("INSERT INTO profiled_launch_artifacts(attempt_id,artifact_id) VALUES($1,$2)")
        .bind(attempt).bind(artifact).execute(&mut *c).await.unwrap();
}

/// A human-owner decision whose `inputs.now_ms` is `now_ms`.
pub async fn decision(c: &mut PgConnection, now_ms: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO intelligence_decisions(decision_id,actor_id,operation_id,payload_digest,decision) VALUES($1,gen_random_uuid(),gen_random_uuid(),repeat('0',64),jsonb_build_object('inputs',jsonb_build_object('now_ms',$2::bigint)))")
        .bind(id).bind(now_ms).execute(&mut *c).await.unwrap();
    id
}

/// A source export: `pending` (charged 1048577) or `verified` (charged `charged`).
pub async fn export(c: &mut PgConnection, project: Uuid, state: &str, charged: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state,verified_bytes,verified_digest) VALUES($1,$2,convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),1048577,$3,$4::text,CASE WHEN $4::text='verified' THEN convert_to('{}','UTF8') END,CASE WHEN $4::text='verified' THEN encode(sha256(convert_to('{}','UTF8')),'hex') END)")
        .bind(id).bind(project).bind(charged).bind(state).execute(&mut *c).await.unwrap();
    id
}

pub async fn link_export(c: &mut PgConnection, attempt: Uuid, export: Uuid) {
    sqlx::query("INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) VALUES($1,$2)")
        .bind(attempt).bind(export).execute(&mut *c).await.unwrap();
}

pub async fn phase_capacity(c: &mut PgConnection, attempt: Uuid, node: Uuid) {
    sqlx::query("INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) VALUES($1,$2)")
        .bind(attempt).bind(node).execute(&mut *c).await.unwrap();
}

pub async fn registration(c: &mut PgConnection, attempt: Uuid, node: Uuid) {
    sqlx::query("INSERT INTO profiled_launch_registrations(attempt_id,registration_id,node_id,boot_id) VALUES($1,gen_random_uuid(),$2,gen_random_uuid())")
        .bind(attempt).bind(node).execute(&mut *c).await.unwrap();
}

/// An inventory report of `node` recorded at `at_ms` (expires 29 s later). Each
/// call needs a distinct `sequence`.
pub async fn inventory(c: &mut PgConnection, node: Uuid, boot: Uuid, sequence: i64, at_ms: i64) {
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) VALUES($1,gen_random_uuid(),'inventory_v2',$2,$3,$3,1,repeat('a',64),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),$4,$4+29000)")
        .bind(node).bind(boot).bind(sequence).bind(at_ms).execute(&mut *c).await.unwrap();
}

/// `sealed_history_usage(kind, scope, now)` as (total_rows, total_bytes, scope_rows, scope_bytes).
pub async fn usage(pool: &PgPool, kind: &str, scope: Option<Uuid>, now_ms: i64, exclude: Option<Uuid>) -> (i64, i64, i64, i64) {
    sqlx::query_as("SELECT total_rows,total_bytes,scope_rows,scope_bytes FROM sealed_history_usage($1,$2,$3,$4)")
        .bind(kind).bind(scope).bind(now_ms).bind(exclude).fetch_one(pool).await.unwrap()
}
```

  In `tests/common/mod.rs`, after `pub mod presence;`, add:

```rust
/// Spec 0008 relational-only seeding for sealed history counting tests.
pub mod sealed;
```

  Run `cargo fmt --all` after writing; formatting differences from the text above are fine.

- [ ] **Step 3: Write `crates/aq-server/tests/it_schema/sealed_history_live.rs`.** Add `mod sealed_history_live;` to `tests/it_schema/main.rs`, in alphabetical position. Every test starts with:

```rust
let db = aq_server::test_support::TestDb::create().await;
let pool = db.pool().await;
```

  It uses `use crate::common::sealed;`, and `const NOW: i64 = 1_800_000_000_000;` at the top of the file. The tests and their exact assertions:

1. `attempt_live_follows_the_hold_and_the_status`. In one `sealed::replica` transaction, seed five attempts:
   - `held = attempt(true, None)`;
   - `held_finished = attempt(true, Some("finished"))`;
   - `released_granted = attempt(false, Some("granted"))`;
   - `released_finished = attempt(false, Some("finished"))`;
   - `released_never = attempt(false, Some("never_granted"))`.

   Commit. Then `SELECT sealed_attempt_live($1)` must return, in order, `true, true, true, false, false`, and `false` for `Uuid::new_v4()`.
2. `artifact_usage_counts_live_links_and_pending_rows_only`. Projects `p = Uuid::new_v4()` and `q = Uuid::new_v4()`, attempts `live = attempt(true, None)` and `settled = attempt(false, Some("finished"))`. Artifacts:
   - `a1 = artifact(p, NOW-5_000_000)` linked to both `settled` and `live`;
   - `a2 = artifact(p, NOW-5_000_000)` linked to `settled` only;
   - `a3 = artifact(p, NOW-1_000)` unlinked;
   - `a4 = artifact(p, NOW-700_000)` unlinked;
   - `a5 = artifact(q, NOW-1_000)` unlinked.

   Assert `usage(&pool, "admission_artifacts", Some(p), NOW, None) == (3, 3, 2, 2)`.
3. `decision_usage_counts_linked_live_and_pending_only`. Attempts `live`, `settled` as above. Decisions:
   - `d1 = decision(NOW-5_000_000)` with `offer(live, d1, node, p, NOW-5_000_000)`;
   - `d2 = decision(NOW-5_000_000)` with `offer(settled, d2, node, p, NOW-5_000_000)`;
   - `d3 = decision(NOW-1_000)`, unlinked;
   - `d4 = decision(NOW-700_000)`, unlinked.

   Assert `usage(.., "intelligence_decisions", None, NOW, None)` has `.0 == 2` and `.2 == 2`.
4. `export_usage_counts_pending_and_live_linked_exports`. Attempts `live`, `settled`. Exports:
   - `e1 = export(p, "pending", 1048577)`;
   - `e2 = export(p, "verified", 5000)` linked to `live`;
   - `e3 = export(p, "verified", 7000)` linked to `settled`;
   - `e4 = export(p, "verified", 9000)`, unlinked.

   Assert:
   - `usage(.., "source_exports", Some(p), 0, None) == (2, 4194310, 2, 4194310)`;
   - `usage(.., "staging_exports", None, 0, None).1 == 1053577`;
   - `usage(.., "source_exports", Some(p), 0, Some(e1)).2 == 1`;
   - `SELECT sealed_staging_live_bytes()` returns `1053577` (no transfers or bases are seeded).
5. `phase_terminal_and_registration_usage_count_live_attempts`. Node `n = Uuid::new_v4()`; attempts `live`, `settled`. Seed `phase_capacity(live, n)`, `phase_capacity(settled, n)`, `registration(live, n)`, `registration(settled, n)`. Assert:
   - `usage(.., "phase_terminal", Some(n), 0, None) == (1, 1048576, 1, 1048576)`;
   - `usage(.., "launch_registrations", Some(n), 0, None) == (1, 64, 1, 64)`.
6. `presence_usage_counts_unexpired_reports_and_the_hour`. Node `n`, boot `b`. Seed `inventory(n, b, 1, NOW-1_000)`, `inventory(n, b, 2, NOW-60_000)`, `inventory(n, b, 3, NOW-60_000)`, `inventory(n, b, 4, NOW-3_700_000)`. Assert:
   - `usage(.., "presence_operations", Some(n), NOW, None) == (1, 1028, 1, 1028)`;
   - `usage(.., "presence_rate", Some(n), NOW, None).2 == 3`.
7. `offer_usage_counts_the_last_hour_per_node_and_project`. Nodes `n`, `m`; projects `p`, `q`. Use `sealed::offers`:
   - 3 offers `(n, p, NOW-1_000)`;
   - 2 offers `(n, p, NOW-3_700_000)`;
   - 1 offer `(m, q, NOW-1_000)`.

   Assert `usage(.., "offers", Some(n), NOW, None) == (4, 0, 3, 0)` and `usage(.., "project_offers", Some(p), NOW, None) == (4, 0, 3, 0)`.
8. `limits_are_the_spec_values`. `SELECT kind,scope,max_rows,max_bytes,headroom_rows,headroom_bytes FROM sealed_history_limits() ORDER BY kind,scope` into `Vec<(String, String, i64, Option<i64>, i64, i64)>`. It must equal the 20 rows of Step 1 in that order, with `NULL` as `None`.
9. `holds_report_the_offer_rate_and_the_project_artifact_cap`:
   - node `n`, project `p`. `sealed::offers(&pool, n, p, NOW-1_000, 30)`;
   - in a replica transaction, seed 449 `artifact(p, NOW-1_000)`;
   - `SELECT hold_code,hold_kind,hold_scope,hold_unit,hold_used,hold_limit FROM sealed_history_holds($1,NULL,$2)` with `(n, NOW)` must equal exactly `vec![("history_rate_limited","offers","node","rows",30,30)]`;
   - with `(NULL, p)` (`sealed_history_holds(NULL,$1,$2)`) it must equal exactly `vec![("history_cap_reached","admission_artifacts","project","rows",449,512)]`, because 449 + 64 headroom > 512;
   - after seeding 1 more offer for `(n, p, NOW-1_000)`, the node call is unchanged except `hold_used == 31`.

- [ ] **Step 4: Run.**

```sh
timeout 900 cargo nextest run -p aq-server --test it_schema -E 'test(/^sealed_history_live::/)'
```
Expected: `9 tests run: 9 passed`.

```sh
timeout 900 cargo nextest run -p aq-server --test it_schema
```
Expected: all pass. The `store::` tests run the new migration twice (idempotency) and on frozen profiles. If a `store::` test fails naming one of the new indexes or functions as unexpected schema, stop and report it with the test output. Do not change the store validators to make it pass.

- [ ] **Step 5: Commit** `feat(schema): 0103 live sealed-history usage, limits and holds (spec 0008 A)`.

### Task 3: Migration 0104, five quota triggers count live rows

**Files:**
- Create: `crates/aq-server/migrations/0104_sealed_history_live_caps.sql`
- Modify: `crates/aq-server/tests/it_schema/schema33_phase_terminal.rs`, `crates/aq-server/tests/it_profiled/profiled_artifacts.rs`, `crates/aq-server/tests/it_schema/sealed_history_live.rs`, `.config/nextest.toml`

- [ ] **Step 1: Write 0104.** It starts with the comment line `-- Spec 0008 phase A: these quota triggers count live rows only (sealed_history_usage, 0103).`. Then come five function definitions, each copied **verbatim** from its current source, with the header changed from `CREATE FUNCTION` (or `CREATE OR REPLACE FUNCTION`) to `CREATE OR REPLACE FUNCTION` and only the replacements below applied. Do **not** copy the `CREATE TRIGGER` lines; the triggers already exist and keep pointing at these functions. The file ends with `UPDATE meta SET value='104' WHERE key='state_format_version';`.

  **(a) `admission_artifact_insert_guard`**: copy `0025_native_source_observations.sql` from line 2 (`CREATE OR REPLACE FUNCTION admission_artifact_insert_guard()`) through its closing `END $$;`.

  Replace:
```
 SELECT count(*),COALESCE(sum(octet_length(body)),0) INTO total_count,total_bytes FROM admission_artifacts;
 SELECT count(*),COALESCE(sum(octet_length(body)),0) INTO project_count,project_bytes FROM admission_artifacts WHERE owner_key=NEW.owner_key;
```
  with:
```
 SELECT u.total_rows,u.total_bytes,u.scope_rows,u.scope_bytes INTO total_count,total_bytes,project_count,project_bytes
 FROM sealed_history_usage('admission_artifacts',CASE WHEN NEW.owner_key ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' THEN NEW.owner_key::UUID END,NEW.created_at_ms) u;
```

  **(b) `profiled_source_export_guard`**: copy `0022_profiled_source_exports.sql` lines 16 (`CREATE FUNCTION profiled_source_export_guard()`) through its closing `END $$;` (line 66). Three replacements:

  1. Replace:
```
DECLARE q JSONB; v JSONB; n BIGINT; total_metadata BIGINT; project_metadata BIGINT; incoming_metadata BIGINT;
```
  with:
```
DECLARE q JSONB; v JSONB; n BIGINT; pn BIGINT; total_metadata BIGINT; project_metadata BIGINT; incoming_metadata BIGINT;
```
  2. Replace:
```
 SELECT COALESCE(sum(octet_length(request_bytes)+COALESCE(octet_length(verified_bytes),4194304)),0),
    COALESCE(sum(octet_length(request_bytes)+COALESCE(octet_length(verified_bytes),4194304)) FILTER(WHERE project_id=NEW.project_id),0)
 INTO total_metadata,project_metadata FROM profiled_source_exports WHERE operation_id<>NEW.operation_id;
```
  with:
```
 SELECT u.total_bytes,u.scope_bytes INTO total_metadata,project_metadata
 FROM sealed_history_usage('source_exports',NEW.project_id,0,NEW.operation_id) u;
```
  3. Replace:
```
  SELECT count(*) INTO n FROM profiled_source_exports;
  IF n>=4096 OR (SELECT count(*) FROM profiled_source_exports WHERE project_id=NEW.project_id)>=256
```
  with:
```
  SELECT u.total_rows,u.scope_rows INTO n,pn FROM sealed_history_usage('source_exports',NEW.project_id,0) u;
  IF n>=4096 OR pn>=256
```

  **(c) `profiled_launch_registration_guard`**: copy `0028_profiled_launch_closure.sql` line 60 through its closing `END $$;` (line 73).

  Replace:
```
 SELECT count(*),COALESCE(sum(logical_bytes),0),count(*) FILTER(WHERE node_id=NEW.node_id),COALESCE(sum(logical_bytes) FILTER(WHERE node_id=NEW.node_id),0) INTO n,b,per_n,per_b FROM profiled_launch_registrations;
```
  with:
```
 SELECT u.total_rows,u.total_bytes,u.scope_rows,u.scope_bytes INTO n,b,per_n,per_b FROM sealed_history_usage('launch_registrations',NEW.node_id,0) u;
```

  **(d) `profiled_phase_terminal_capacity_guard`**: copy `0033_phase_terminal.sql` line 15 through its closing `END $$;` (line 25).

  Replace:
```
 SELECT count(*),COALESCE(sum(reserved_bytes),0),count(*) FILTER(WHERE node_id=NEW.node_id),COALESCE(sum(reserved_bytes) FILTER(WHERE node_id=NEW.node_id),0) INTO n,b,pn,pb FROM profiled_phase_terminal_capacity;
```
  with:
```
 SELECT u.total_rows,u.total_bytes,u.scope_rows,u.scope_bytes INTO n,b,pn,pb FROM sealed_history_usage('phase_terminal',NEW.node_id,0) u;
```

  **(e) `profiled_presence_operation_guard`**: copy `0029_scoped_material_presence.sql` line 70 through its closing `END $$;` (line 97). Two replacements:

  1. Replace:
```
DECLARE g profiled_presence_storage%ROWTYPE; n profiled_presence_node_storage%ROWTYPE; incoming BIGINT; q JSONB; a JSONB;
```
  with:
```
DECLARE g profiled_presence_storage%ROWTYPE; n profiled_presence_node_storage%ROWTYPE; incoming BIGINT; q JSONB; a JSONB; u RECORD; r RECORD;
```
  2. Replace:
```
 IF g.operations>=65536 OR n.operations>=4096 OR g.logical_bytes+incoming>268435456 OR n.logical_bytes+incoming>16777216
 THEN RAISE EXCEPTION 'scoped presence retention capacity' USING ERRCODE='54000'; END IF;
```
  with:
```
 SELECT * INTO u FROM sealed_history_usage('presence_operations',NEW.node_id,NEW.recorded_at_ms);
 SELECT * INTO r FROM sealed_history_usage('presence_rate',NEW.node_id,NEW.recorded_at_ms);
 IF u.total_rows>=65536 OR u.scope_rows>=4096 OR u.total_bytes+incoming>268435456 OR u.scope_bytes+incoming>16777216
 OR r.total_rows>=11520 OR r.scope_rows>=720
 THEN RAISE EXCEPTION 'scoped presence retention capacity' USING ERRCODE='54000'; END IF;
```
  The two `UPDATE profiled_presence_storage ...` / `UPDATE profiled_presence_node_storage ...` counter lines stay; they are statistics now.

  Check each replacement's "find" text is present exactly once in the copied body before replacing (`grep -c`). If one is not found, stop and report; do not improvise.

- [ ] **Step 2: Update `schema33_phase_terminal::actual_grant_cannot_commit_without_reserved_terminal_capacity`.** The 16 seeded capacity rows must belong to live attempts, or they no longer count. Replace the line:

```rust
    sqlx::query("INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) SELECT gen_random_uuid(),$1 FROM generate_series(1,16)").bind(f.prepared.original.identity.node_id).execute(&mut *occupied).await.unwrap();
```
  with:
```rust
    // Spec 0008: capacity counts live attempts only, so the occupants hold their tasks.
    sqlx::query("WITH a AS (INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) SELECT gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),TRUE,'{}'::jsonb FROM generate_series(1,16) RETURNING attempt_id) INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) SELECT attempt_id,$1 FROM a").bind(f.prepared.original.identity.node_id).execute(&mut *occupied).await.unwrap();
```
  The assertions stay as they are (`counts == (16, 0)`).

- [ ] **Step 3: Add `schema33_phase_terminal::settled_terminal_capacity_does_not_block_a_new_grant`** directly below that test:
  - the same body, but the seeded attempts are released and finished: `FALSE,'{"status":"finished"}'::jsonb` instead of `TRUE,'{}'::jsonb`;
  - the assertion becomes `assert!(result.is_ok(), "settled capacity is not live: {result:?}")`;
  - the counts query asserts `counts.0 == 17`;
  - drop the `fixture::assert_unissued` line.

  Add its exact name to the `selection-fixture` filter in `.config/nextest.toml`, inside the existing `binary_id(aq-server::it_schema) & (...)` clause, after `test(=schema33_phase_terminal::actual_grant_cannot_commit_without_reserved_terminal_capacity)`:

```
 | test(=schema33_phase_terminal::settled_terminal_capacity_does_not_block_a_new_grant)
```

- [ ] **Step 4: Add `profiled_artifacts::orphaned_and_settled_artifacts_do_not_count_toward_the_project_cap`** to `tests/it_profiled/profiled_artifacts.rs`:

```rust
#[tokio::test]
async fn orphaned_and_settled_artifacts_do_not_count_toward_the_project_cap() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let pool = f.db.pool().await;
    let now = NOW as i64;
    // 600 unlinked rows older than the 10-minute pending window: orphans.
    let mut tx = crate::common::sealed::replica(&pool).await;
    for _ in 0..600 {
        crate::common::sealed::artifact(&mut tx, project, now - 700_000).await;
    }
    tx.commit().await.unwrap();
    let a = artifact("live-cap-context", b"x");
    let retain = |id| Retain {
        artifact_id: id,
        project_id: project,
        owner: OwnerKind::GraphSnapshot,
        artifact: a.clone(),
        kind: ArtifactKind::Context,
        bytes: b"x",
        retain_until_ms: NOW + 60_000,
    };
    artifacts::retain(&f.state, retain(Uuid::new_v4()))
        .await
        .expect("orphans are not live");
    // 511 pending rows plus the one just retained make 512 live rows.
    let mut tx = crate::common::sealed::replica(&pool).await;
    for _ in 0..511 {
        crate::common::sealed::artifact(&mut tx, project, now - 1_000).await;
    }
    tx.commit().await.unwrap();
    assert!(artifacts::retain(&f.state, retain(Uuid::new_v4())).await.is_err());
}
```
  If `retain` as a closure does not compile because `Retain` borrows `a`, write the two `Retain { .. }` literals out in full.

- [ ] **Step 5: Add `sealed_history_live::phase_a_guards_call_the_live_usage`** to the it_schema module:
  - for each `(function, kind)` in `[("admission_artifact_insert_guard","admission_artifacts"), ("profiled_source_export_guard","source_exports"), ("profiled_launch_registration_guard","launch_registrations"), ("profiled_phase_terminal_capacity_guard","phase_terminal"), ("profiled_presence_operation_guard","presence_rate")]`, assert that `SELECT prosrc FROM pg_proc WHERE proname=$1` contains `format!("sealed_history_usage('{kind}'")`;
  - assert that `SELECT count(*) FROM pg_trigger WHERE NOT tgisinternal AND tgname IN ('admission_artifact_insert','profiled_source_export_guard','profiled_launch_registration_guard','profiled_phase_terminal_capacity_guard','profiled_presence_operation_guard')` is `5`.

- [ ] **Step 6: Run.**

```sh
timeout 900 cargo nextest run -p aq-server --test it_schema
timeout 900 cargo nextest run -p aq-server --test it_profiled -E 'test(/^profiled_artifacts::/)'
timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_source::/) | test(/^scoped_presence_inventory::/) | test(/^native_initial_grant::/) | test(/^native_offers::/)'
```
Expected: all pass, including `schema33_phase_terminal::settled_terminal_capacity_does_not_block_a_new_grant` and `profiled_artifacts::orphaned_and_settled_artifacts_do_not_count_toward_the_project_cap`. The existing `profiled_artifacts::finite_project_history_refuses_new_rows_without_evicting_or_blocking_exact_replay` still passes, because its rows are written at `NOW` and are pending.

- [ ] **Step 7: Commit** `feat(schema): 0104 quota triggers count live rows (spec 0008 A)`.

### Task 4: The `aq-history-caps` crate

**Files:**
- Create: `crates/aq-history-caps/Cargo.toml`, `crates/aq-history-caps/src/lib.rs`
- Modify: workspace `Cargo.toml` (`members`), `Cargo.lock` (by cargo)

- [ ] **Step 1: `Cargo.toml`:**

```toml
[package]
name = "aq-history-caps"
version = "0.1.0"
edition.workspace = true
publish = false
description = "Spec 0008 phase A: live sealed-history usage, the poll gate and swarm holds"

[dependencies]
sqlx.workspace = true
sqlx-postgres.workspace = true
uuid.workspace = true
```
  Add `"crates/aq-history-caps"` to `members` in the workspace `Cargo.toml`, before `"crates/aq-server"`.

- [ ] **Step 2: `src/lib.rs`, exactly** (then `cargo fmt`):

```rust
//! Spec 0008 phase A: live sealed-history usage, the poll gate and swarm holds.
//!
//! Migration 0103's SQL functions are the only definition of a live row. The
//! database triggers, these Rust mirror checks, the poll gate and the swarm read
//! all call them. Before 0103 (frozen-format fixtures) every row counts, as it
//! did before spec 0008.
use sqlx::PgConnection;
use uuid::Uuid;

const LIVE_SCHEMA: &str =
    "SELECT to_regprocedure('sealed_history_usage(text,uuid,bigint,uuid)') IS NOT NULL";
const LEGACY_STAGING: &str = "SELECT (COALESCE((SELECT sum((manifest->>'manifest_bytes')::BIGINT+(manifest->>'bundle_bytes')::BIGINT) FROM source_transfers WHERE NOT staging_released),0)+COALESCE((SELECT sum(reserved_bytes) FROM source_bases),0)+COALESCE((SELECT sum(charged_bytes) FROM profiled_source_exports),0))::BIGINT";

/// The usage kinds Rust checks mirror (the SQL function knows more).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    IntelligenceDecisions,
    SourceExports,
    NativePreparation,
    V2Proposals,
    LaunchRegistrations,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::IntelligenceDecisions => "intelligence_decisions",
            Kind::SourceExports => "source_exports",
            Kind::NativePreparation => "native_preparation",
            Kind::V2Proposals => "v2_proposals",
            Kind::LaunchRegistrations => "launch_registrations",
        }
    }
    /// The all-rows query used before migration 0103; `$1`, when present, is the scope.
    fn legacy(self) -> Option<&'static str> {
        match self {
            Kind::IntelligenceDecisions => Some("SELECT count(*),COALESCE(sum(pg_column_size(decision)),0)::BIGINT,count(*),COALESCE(sum(pg_column_size(decision)),0)::BIGINT FROM intelligence_decisions"),
            Kind::SourceExports => Some("SELECT count(*),COALESCE(sum(octet_length(request_bytes)+COALESCE(octet_length(verified_bytes),4194304)),0)::BIGINT,count(*) FILTER(WHERE project_id=$1),COALESCE(sum(octet_length(request_bytes)+COALESCE(octet_length(verified_bytes),4194304)) FILTER(WHERE project_id=$1),0)::BIGINT FROM profiled_source_exports"),
            Kind::V2Proposals => Some("SELECT count(*),0::BIGINT,count(*) FILTER(WHERE node_id=$1),0::BIGINT FROM profiled_v2_proposals"),
            Kind::LaunchRegistrations => Some("SELECT count(*),COALESCE(sum(logical_bytes)::BIGINT,0),count(*) FILTER(WHERE node_id=$1),COALESCE((sum(logical_bytes) FILTER(WHERE node_id=$1))::BIGINT,0) FROM profiled_launch_registrations"),
            Kind::NativePreparation => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub total_rows: i64,
    pub total_bytes: i64,
    pub scope_rows: i64,
    pub scope_bytes: i64,
}

impl From<(i64, i64, i64, i64)> for Usage {
    fn from((total_rows, total_bytes, scope_rows, scope_bytes): (i64, i64, i64, i64)) -> Self {
        Self { total_rows, total_bytes, scope_rows, scope_bytes }
    }
}

fn ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Whether migration 0103's live definition is present.
pub async fn live_schema(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(LIVE_SCHEMA).fetch_one(conn).await
}

/// Live usage, or `None` before migration 0103.
pub async fn live_usage(
    conn: &mut PgConnection,
    kind: Kind,
    scope: Option<Uuid>,
    now_ms: u64,
) -> Result<Option<Usage>, sqlx::Error> {
    if !live_schema(&mut *conn).await? {
        return Ok(None);
    }
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT total_rows,total_bytes,scope_rows,scope_bytes FROM sealed_history_usage($1,$2,$3)",
    )
    .bind(kind.name())
    .bind(scope)
    .bind(ms(now_ms))
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(row.into()))
}

/// Live usage, or every row before migration 0103.
pub async fn usage(
    conn: &mut PgConnection,
    kind: Kind,
    scope: Option<Uuid>,
    now_ms: u64,
) -> Result<Usage, sqlx::Error> {
    if let Some(u) = live_usage(&mut *conn, kind, scope, now_ms).await? {
        return Ok(u);
    }
    let sql = kind
        .legacy()
        .ok_or_else(|| sqlx::Error::Protocol(format!("{} has no pre-0103 query", kind.name())))?;
    let query = sqlx::query_as::<_, (i64, i64, i64, i64)>(sql);
    let query = if sql.contains("$1") { query.bind(scope) } else { query };
    Ok(query.fetch_one(&mut *conn).await?.into())
}

/// Staging bytes: unreleased transfers, source bases and (live) source exports.
pub async fn staging_used(conn: &mut PgConnection) -> Result<i64, sqlx::Error> {
    let sql = if live_schema(&mut *conn).await? {
        "SELECT sealed_staging_live_bytes()"
    } else {
        LEGACY_STAGING
    };
    sqlx::query_scalar::<_, i64>(sql).fetch_one(&mut *conn).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    RateLimited,
    CapReached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Node(Uuid),
    Project(Uuid),
    Global,
}

/// One limit that holds new offers back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    pub code: Code,
    /// A `sealed_history_limits()` kind, or `staging_exports`.
    pub kind: String,
    pub scope: Scope,
    pub bytes: bool,
    pub used: i64,
    pub limit: i64,
}

/// (max_staging_bytes, max_bundle_bytes) from the source retention config.
pub type Staging = (u64, u64);

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

impl Hold {
    pub fn code_name(&self) -> &'static str {
        match self.code {
            Code::RateLimited => "history_rate_limited",
            Code::CapReached => "history_cap_reached",
        }
    }
    pub fn subject(&self) -> &'static str {
        match (self.kind.as_str(), self.bytes) {
            ("admission_artifacts", _) => "admission artifacts",
            ("intelligence_decisions", _) => "selection decisions",
            ("source_exports", _) => "source exports",
            ("staging_exports", _) => "staging bytes",
            ("native_preparation", true) => "preparation bytes",
            ("native_preparation", false) => "material leases",
            ("v2_proposals", _) => "preparation proposals",
            ("launch_registrations", _) => "launch registrations",
            ("phase_terminal", _) => "phase terminal reservations",
            ("presence_operations", _) => "inventory reports",
            _ => "sealed history",
        }
    }
    pub fn message(&self) -> String {
        let (used, limit) = (self.used, self.limit);
        let body = match (self.code, self.kind.as_str()) {
            (Code::RateLimited, "presence_rate") => format!(
                "inventory report rate limit reached: {used} reports in the last hour (limit {limit})"
            ),
            (Code::RateLimited, _) => {
                format!("offer rate limit reached: {used} offers in the last hour (limit {limit})")
            }
            (Code::CapReached, _) => format!(
                "live {} cap reached: {used} of {limit}{}",
                self.subject(),
                if self.bytes { " bytes" } else { "" }
            ),
        };
        match self.scope {
            Scope::Node(_) => capitalised(&body),
            Scope::Project(p) => format!("Project {p}: {body}"),
            Scope::Global => format!("Server: {body}"),
        }
    }
    pub fn remedy(&self) -> &'static str {
        match self.code {
            Code::RateLimited => {
                "Wait for the hour to pass; if this repeats, look for a task that fails in a loop"
            }
            Code::CapReached => "Wait for running agents to settle",
        }
    }
    pub fn log_line(&self) -> String {
        format!("{} {} {:?}: {} of {}", self.code_name(), self.kind, self.scope, self.used, self.limit)
    }
}

async fn sql_holds(
    conn: &mut PgConnection,
    node: Option<Uuid>,
    project: Option<Uuid>,
    now_ms: u64,
) -> Result<Vec<Hold>, sqlx::Error> {
    let rows: Vec<(String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT hold_code,hold_kind,hold_scope,hold_unit,hold_used,hold_limit FROM sealed_history_holds($1,$2,$3)",
    )
    .bind(node)
    .bind(project)
    .bind(ms(now_ms))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(code, kind, scope, unit, used, limit)| Hold {
            code: if code == "history_rate_limited" { Code::RateLimited } else { Code::CapReached },
            scope: match (scope.as_str(), node, project) {
                ("node", Some(n), _) => Scope::Node(n),
                ("project", _, Some(p)) => Scope::Project(p),
                _ => Scope::Global,
            },
            kind,
            bytes: unit == "bytes",
            used,
            limit,
        })
        .collect())
}

/// Node and server limits for a poll of `node`. Empty before migration 0103.
pub async fn node_holds(
    conn: &mut PgConnection,
    node: Uuid,
    now_ms: u64,
    staging: Option<Staging>,
) -> Result<Vec<Hold>, sqlx::Error> {
    if !live_schema(&mut *conn).await? {
        return Ok(Vec::new());
    }
    let mut holds = sql_holds(&mut *conn, Some(node), None, now_ms).await?;
    if let Some((max, bundle)) = staging {
        let used = staging_used(&mut *conn).await?;
        let limit = ms(max);
        if used.saturating_add(ms(bundle)) > limit {
            holds.push(Hold {
                code: Code::CapReached,
                kind: "staging_exports".into(),
                scope: Scope::Global,
                bytes: true,
                used,
                limit,
            });
        }
    }
    Ok(holds)
}

/// Project limits. Empty before migration 0103.
pub async fn project_holds(
    conn: &mut PgConnection,
    project: Uuid,
    now_ms: u64,
) -> Result<Vec<Hold>, sqlx::Error> {
    if !live_schema(&mut *conn).await? {
        return Ok(Vec::new());
    }
    sql_holds(conn, None, Some(project), now_ms).await
}

/// Every hold the swarm read shows: each listed node's and server holds (once),
/// then each bound project's holds.
pub async fn swarm_holds(
    conn: &mut PgConnection,
    nodes: &[Uuid],
    now_ms: u64,
    staging: Option<Staging>,
) -> Result<Vec<Hold>, sqlx::Error> {
    let mut out: Vec<Hold> = Vec::new();
    if !live_schema(&mut *conn).await? {
        return Ok(out);
    }
    for node in nodes {
        for hold in node_holds(&mut *conn, *node, now_ms, staging).await? {
            if !out.contains(&hold) {
                out.push(hold);
            }
        }
    }
    let projects: Vec<Uuid> =
        sqlx::query_scalar("SELECT project_id FROM project_execution_bindings ORDER BY project_id")
            .fetch_all(&mut *conn)
            .await?;
    for project in projects {
        out.extend(project_holds(&mut *conn, project, now_ms).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(code: Code, kind: &str, scope: Scope, bytes: bool, used: i64, limit: i64) -> Hold {
        Hold { code, kind: kind.into(), scope, bytes, used, limit }
    }

    #[test]
    fn node_offer_rate_message_and_remedy() {
        let h = hold(Code::RateLimited, "offers", Scope::Node(Uuid::nil()), false, 30, 30);
        assert_eq!(h.message(), "Offer rate limit reached: 30 offers in the last hour (limit 30)");
        assert_eq!(
            h.remedy(),
            "Wait for the hour to pass; if this repeats, look for a task that fails in a loop"
        );
        assert_eq!(h.code_name(), "history_rate_limited");
    }

    #[test]
    fn inventory_rate_message() {
        let h = hold(Code::RateLimited, "presence_rate", Scope::Node(Uuid::nil()), false, 720, 720);
        assert_eq!(
            h.message(),
            "Inventory report rate limit reached: 720 reports in the last hour (limit 720)"
        );
    }

    #[test]
    fn project_cap_message() {
        let h = hold(Code::CapReached, "admission_artifacts", Scope::Project(Uuid::from_u128(1)), false, 449, 512);
        assert_eq!(
            h.message(),
            "Project 00000000-0000-0000-0000-000000000001: live admission artifacts cap reached: 449 of 512"
        );
        assert_eq!(h.remedy(), "Wait for running agents to settle");
        assert_eq!(h.code_name(), "history_cap_reached");
    }

    #[test]
    fn server_bytes_message() {
        let h = hold(Code::CapReached, "native_preparation", Scope::Global, true, 4_194_000, 4_194_304);
        assert_eq!(h.message(), "Server: live preparation bytes cap reached: 4194000 of 4194304 bytes");
        let leases = hold(Code::CapReached, "native_preparation", Scope::Node(Uuid::nil()), false, 256, 256);
        assert_eq!(leases.message(), "Live material leases cap reached: 256 of 256");
    }

    #[test]
    fn kinds_have_names_and_only_preparation_lacks_a_legacy_query() {
        for kind in [Kind::IntelligenceDecisions, Kind::SourceExports, Kind::V2Proposals, Kind::LaunchRegistrations] {
            assert!(kind.legacy().is_some(), "{kind:?}");
        }
        assert!(Kind::NativePreparation.legacy().is_none());
        assert_eq!(Kind::SourceExports.name(), "source_exports");
    }
}
```

- [ ] **Step 3: Run.**

```sh
timeout 900 cargo test -p aq-history-caps
```
Expected: `test result: ok. 5 passed`. This also updates `Cargo.lock` (commit it). Then:

```sh
timeout 900 cargo clippy -p aq-history-caps --all-targets -- -D warnings
```
Expected: no warnings.

- [ ] **Step 4: Commit** `feat(history-caps): live sealed-history usage, holds and messages (spec 0008 A)`.

### Task 5: Rust mirror checks use live usage

**Files:** `crates/aq-server/Cargo.toml` and the six source files named below.

- [ ] **Step 1:** In `crates/aq-server/Cargo.toml` `[dependencies]`, add `aq-history-caps = { path = "../aq-history-caps", version = "=0.1.0" }` next to `aq-swarm`.

- [ ] **Step 2: `src/intelligence/resolver.rs` (decisions, around line 340).** Replace:

```rust
    let row=sqlx::query("SELECT count(*) AS entries,COALESCE(sum(pg_column_size(decision)),0)::BIGINT AS bytes FROM intelligence_decisions").fetch_one(&mut *tx).await.map_err(db)?;
```
  with:
```rust
    let kind = aq_history_caps::Kind::IntelligenceDecisions;
    let row = aq_history_caps::usage(&mut tx, kind, None, decision.inputs.now_ms)
        .await
        .map_err(db)?;
```
  and in the condition below it replace `row.get::<i64, _>("entries")` with `row.total_rows` and `row.get::<i64, _>("bytes")` with `row.total_bytes`. The `incoming` computation, the limits (1024, 64 MiB) and the error text stay. If `sqlx::Row` becomes unused in this file, remove its import.

- [ ] **Step 3: `src/profiled_execution/closure/history.rs` (registrations, line 151).** Replace:

```rust
    let quota:(i64,i64,i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(logical_bytes)::BIGINT,0),count(*) FILTER(WHERE node_id=$1),COALESCE((sum(logical_bytes) FILTER(WHERE node_id=$1))::BIGINT,0) FROM profiled_launch_registrations").bind(node_id).fetch_one(&mut **tx).await.map_err(database)?;
```
  with:
```rust
    let kind = aq_history_caps::Kind::LaunchRegistrations;
    let u = aq_history_caps::usage(&mut **tx, kind, Some(node_id), 0)
        .await
        .map_err(database)?;
    let quota = (u.total_rows, u.total_bytes, u.scope_rows, u.scope_bytes);
```
  Leave the registration history check at lines 114-122 unchanged (spec §5.2, deferred to phase B).

- [ ] **Step 4: `src/profiled_execution/source_exports.rs` (staging line 331, capacity lines 340-355).**
  - Replace the `let used:i64=sqlx::query_scalar("SELECT (COALESCE((SELECT sum((manifest->>'manifest_bytes')::BIGINT+...` line with `let used = aq_history_caps::staging_used(&mut tx).await.map_err(database)?;`.
  - Replace the `let capacity=sqlx::query("SELECT count(*) AS total_count,...").bind(request.project_id).fetch_one(&mut *tx).await.map_err(database)?;` line and the `if` condition that follows with:

```rust
        let kind = aq_history_caps::Kind::SourceExports;
        let capacity = aq_history_caps::usage(&mut tx, kind, Some(request.project_id), 0)
            .await
            .map_err(database)?;
        let incoming = request_bytes.len() as i64 + 4_194_304;
        if capacity.total_rows >= 4096
            || capacity.scope_rows >= 256
            || capacity.total_bytes.checked_add(incoming).is_none_or(|n| n > 268_435_456)
            || capacity.scope_bytes.checked_add(incoming).is_none_or(|n| n > 67_108_864)
        {
```
  (The existing `let incoming = ...` line moves inside this block, unchanged.) The error body `"source export metadata quota exhausted"` stays.

- [ ] **Step 5: `src/source_retention.rs` (lines 397 and 915).** Replace each of the two `let used:i64=sqlx::query_scalar("SELECT (COALESCE((SELECT sum((manifest->>'manifest_bytes')::bigint+...` lines with:

```rust
    let used = aq_history_caps::staging_used(&mut tx).await.map_err(internal)?;
```
  Indent to match the surrounding code.

- [ ] **Step 6: `src/profiled_execution/preparation.rs` (`retained_quota_locked`, line 480).** Insert as the first statement of the function body:

```rust
    let kind = aq_history_caps::Kind::NativePreparation;
    if let Some(u) = aq_history_caps::live_usage(&mut **tx, kind, Some(node), 0)
        .await
        .map_err(db)?
    {
        return Ok((u.total_rows, u.scope_rows, u.total_bytes, u.scope_bytes));
    }
```
  The rest of the function (the pre-0103 path) stays. The tuple order is (total leases, node leases, total bytes, node bytes), as the four callers expect.

- [ ] **Step 7: `src/profiled_execution/accepted_preparation.rs` (`quota`, lines 65-72).** Replace:

```rust
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER(WHERE node_id=$1) FROM profiled_v2_proposals",
    )
    .bind(node)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
```
  with:
```rust
    let kind = aq_history_caps::Kind::V2Proposals;
    let p = aq_history_caps::usage(&mut **tx, kind, Some(node), 0).await.map_err(db)?;
    let counts = (p.total_rows, p.scope_rows);
```

  In Steps 2–7, `&mut tx` for a local `Transaction` and `&mut **tx` for a `&mut Transaction` parameter both give the `&mut PgConnection` the crate takes. If the compiler disagrees, use the form the replaced line used (`&mut *tx` / `&mut **tx`).

- [ ] **Step 8: Run.**

```sh
timeout 900 cargo nextest run -p aq-server --test it_native --test it_profiled --test it_schema --no-fail-fast
```
Expected: all pass. The `native_accepted_preparation` byte-budget tests seed legacy `profiled_execution_receipts`, which still count. If one fails, report the test and output; do not change its expectation.

```sh
python3 scripts/build-budget.py
```
Expected: `OK`.

- [ ] **Step 9: Commit** `feat(caps): Rust mirror checks count live sealed history (spec 0008 A)`.

### Task 6: Migration 0105, the offer-rate fence

**Files:**
- Create: `crates/aq-server/migrations/0105_sealed_offer_rate.sql`
- Modify: `crates/aq-server/tests/it_schema/sealed_history_live.rs`

- [ ] **Step 1: Write 0105 exactly:**

```sql
-- Spec 0008 phase A: at most 30 offers per node, 240 per project and 960 in all in
-- any rolling hour, measured at each offer's source observation. The poll gate
-- holds visibly first; this fence only catches a race between two polls.
CREATE FUNCTION profiled_launch_offer_rate() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE offered BIGINT; project UUID; n RECORD; p RECORD;
BEGIN
 SELECT o.observed_at_ms,o.project_id INTO offered,project FROM admission_source_observations o WHERE o.observation_id=NEW.source_observation_id;
 IF NOT FOUND THEN RETURN NEW; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-sealed-offer-rate-v1',0));
 SELECT * INTO n FROM sealed_history_usage('offers',(NEW.launch_binding#>>'{session,node_id}')::UUID,offered);
 SELECT * INTO p FROM sealed_history_usage('project_offers',project,offered);
 IF n.scope_rows>=30 OR n.total_rows>=960 OR p.scope_rows>=240 THEN
  RAISE EXCEPTION 'sealed history offer rate' USING ERRCODE='54000';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_launch_offer_rate BEFORE INSERT ON profiled_launches FOR EACH ROW EXECUTE FUNCTION profiled_launch_offer_rate();
UPDATE meta SET value='105' WHERE key='state_format_version';
```

- [ ] **Step 2: Add `sealed_history_live::offer_rate_fence_refuses_the_thirty_first_offer_in_an_hour`:**
  - node `n`, project `p`. `sealed::offers(&pool, n, p, NOW-1_000, 30)`;
  - open a transaction:
    - `SET LOCAL session_replication_role='replica'`;
    - insert one observation at `NOW` for `p` (the first `INSERT` of `sealed::offer`'s body, with a new `observation` id);
    - `SET LOCAL session_replication_role='origin'`;
    - insert the launch for that observation on node `n` (the second `INSERT` of `sealed::offer`'s body);
  - the insert must fail with `as_database_error().unwrap().code().as_deref() == Some("54000")` and a message containing `sealed history offer rate`;
  - repeat with a node `m` that has 29 seeded offers. The insert must fail with code `Some("23503")` (the decision FK, checked after the fence). That shows the fence passed.

- [ ] **Step 3: Run.**

```sh
timeout 900 cargo nextest run -p aq-server --test it_schema -E 'test(/^sealed_history_live::/)'
timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::/)'
```
Expected: `11 passed` for the first. The second is all green (one real offer per test stays far under the limit).

- [ ] **Step 4: Commit** `feat(schema): 0105 offer rate fence on profiled launches (spec 0008 A)`.

### Task 7: The poll gate

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs`, `crates/aq-server/tests/it_native/native_offers.rs`
- Create: `crates/aq-server/tests/it_native/native_offers/history_caps.rs`

- [ ] **Step 1: Node gate.** In `offers()`, directly after:

```rust
    if busy {
        return Ok(page);
    }
```
  insert:
```rust
    // Spec 0008: a history cap or the offer rate holds the node, visibly, before any write.
    let staging = (state.source_retention.as_ref())
        .map(|r| (r.config.max_staging_bytes, r.config.max_bundle_bytes));
    let mut conn = state.store.pool.acquire().await.map_err(|_| refused())?;
    let holds = aq_history_caps::node_holds(&mut conn, auth.id(), now(state)?, staging).await;
    if let Some(h) = holds.map_err(|_| refused())?.first() {
        tracing::info!(node_id = %auth.id(), "{}; no new offers", h.log_line());
        return Ok(page);
    }
    drop(conn);
```

- [ ] **Step 2: Project skip.** Inside the `for candidate in fairness::order(...)` loop, directly after the `let policy = config ... .ok_or_else(|| refused())?;` statement, insert:

```rust
            let mut conn = state.store.pool.acquire().await.map_err(|_| refused())?;
            let held = aq_history_caps::project_holds(&mut conn, policy.project_id, polled_at);
            if !held.await.map_err(|_| refused())?.is_empty() {
                continue;
            }
            drop(conn);
```

- [ ] **Step 3: Tests.** Add to `tests/it_native/native_offers.rs`, next to the other `#[path]` modules:

```rust
#[path = "native_offers/history_caps.rs"]
mod history_caps;
```

  Write `native_offers/history_caps.rs`:

```rust
//! Spec 0008 phase A: the offer rate holds a node with an empty poll, never a 409,
//! and writes no history; offers older than an hour do not count.
use crate::common::{native_offer, sealed};
use aq_admission::control::{ControlReply, decode_reply};
use aq_server::worker_api;
use uuid::Uuid;

async fn node(c: &native_offer::Case) -> Uuid {
    let mut headers = axum::http::HeaderMap::new();
    let bearer = format!("Bearer {}", c.env.env.credential);
    headers.insert("authorization", bearer.parse().unwrap());
    worker_api::authenticate_context(&c.env.env.state, &headers).await.unwrap().id()
}
fn offered(body: &serde_json::Value) -> usize {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    page.offers.len()
}
async fn rows(pool: &sqlx::PgPool) -> (i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM intelligence_decisions),(SELECT count(*) FROM admission_artifacts)")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn offer_rate_limit_holds_the_node_without_a_409() {
    let c = native_offer::setup().await;
    let pool = &c.env.env.pool;
    let now = (c.env.env.state.clock)().timestamp_millis();
    let id = node(&c).await;
    sealed::offers(pool, id, c.env.lineage.project_id, now - 1_000, 30).await;
    let before = rows(pool).await;
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "never a 409: {body}");
    assert_eq!(offered(&body), 0, "{body}");
    assert_eq!(rows(pool).await, before, "a held poll writes no history");
    let mut conn = pool.acquire().await.unwrap();
    let holds = aq_history_caps::node_holds(&mut conn, id, now as u64, None).await.unwrap();
    assert_eq!(
        holds.iter().map(|h| h.message()).collect::<Vec<_>>(),
        vec!["Offer rate limit reached: 30 offers in the last hour (limit 30)".to_string()]
    );
}

#[tokio::test]
async fn offers_older_than_an_hour_do_not_count() {
    let c = native_offer::setup().await;
    let pool = &c.env.env.pool;
    let now = (c.env.env.state.clock)().timestamp_millis();
    let id = node(&c).await;
    sealed::offers(pool, id, c.env.lineage.project_id, now - 3_700_000, 30).await;
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered(&body), 1, "{body}");
}
```
  Add `aq-history-caps` to aq-server's `[dev-dependencies]` only if the test cannot see it through `[dependencies]` (normal dependencies are visible to integration tests, so this is normally not needed). If `c.env.lineage` or `c.env.env.credential` have other names in `tests/common/native_offer.rs`, use the names that `native_offers/lost.rs` uses for the same values.

  If `offers_older_than_an_hour_do_not_count` fails because a reader trips over the seeded launch rows (they have no `execution_attempts` row), report the failing reader and its error. Do not weaken the assertion.

- [ ] **Step 4: Run.**

```sh
timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::/)'
```
Expected: all pass, including the two new tests (the module regex already puts them in `selection-fixture`).

```sh
python3 scripts/build-budget.py
```
Expected: `OK`.

- [ ] **Step 5: Commit** `feat(poll): hold offers on a history cap or the offer rate, visibly (spec 0008 A)`.

### Task 8: Swarm holds and the contract

**Files:**
- Modify: `crates/aq-dashboard-contract/src/swarm_contract.rs`, `crates/aq-swarm/Cargo.toml`, `crates/aq-swarm/src/assemble.rs`, `crates/aq-swarm/src/tests.rs`, `crates/aq-server/src/dashboard_api/swarm.rs`, `protocol/dashboard-read/v1/schema.json`, `protocol/dashboard-read/v1/README.md`, `crates/aq-server/tests/it_dashboard/dashboard_swarm.rs`

- [ ] **Step 1: Contract.** In `swarm_contract.rs` `pub enum HoldCode`, after `NoFreeSlot,` add:

```rust
    /// Spec 0008: an offer or inventory report rate limit holds new work.
    HistoryRateLimited,
    /// Spec 0008: a live sealed-history cap holds new work.
    HistoryCapReached,
```
  If a test in that file lists every code, add the two (`history_rate_limited`, `history_cap_reached`).

- [ ] **Step 2: aq-swarm.**
  - `Cargo.toml`: add `aq-history-caps = { path = "../aq-history-caps", version = "=0.1.0" }`.
  - `assemble.rs`:
    1. In `pub struct SwarmInput`, add the last field, `pub history: Vec<aq_history_caps::Hold>,`.
    2. In `hold()`, add these two match arms before `};`:

```rust
        HistoryRateLimited => (
            "Offer rate limit reached",
            Some("Wait for the hour to pass; if this repeats, look for a task that fails in a loop"),
        ),
        HistoryCapReached => (
            "Live history cap reached",
            Some("Wait for running agents to settle"),
        ),
```
    3. Add below `hold()`:

```rust
/// A spec 0008 history hold with its own counted message.
pub fn history_reason(h: &aq_history_caps::Hold) -> HoldReason {
    HoldReason {
        code: match h.code {
            aq_history_caps::Code::RateLimited => HoldCode::HistoryRateLimited,
            aq_history_caps::Code::CapReached => HoldCode::HistoryCapReached,
        },
        message: h.message(),
        remedy: Some(h.remedy().into()),
    }
}
```
    4. In `assemble()`, add `history,` to the `let SwarmInput { ... } = input;` destructuring.
    5. Inside the `if n.health == WorkerHealth::Online { ... }` block, after the `match service.map(|s| s.state) { ... }`, collect:

```rust
            node_history.extend(
                history
                    .iter()
                    .filter(|h| h.scope == aq_history_caps::Scope::Node(id))
                    .map(history_reason),
            );
```
       Declare `let mut node_history: Vec<HoldReason> = Vec::new();` next to `let mut codes`.
    6. Change `holding: codes.into_iter().map(hold).collect(),` to `holding: codes.into_iter().map(hold).chain(node_history).collect(),`.
    7. Replace `let reasons = waiting(&out);` with:

```rust
    let mut reasons = waiting(&out);
    reasons.extend(
        history
            .iter()
            .filter(|h| !matches!(h.scope, aq_history_caps::Scope::Node(_)))
            .map(history_reason),
    );
```
  - `tests.rs`:
    - add `history: Vec::new(),` to both `SwarmInput { ... }` builders (`input` and `service_input`);
    - add the test:

```rust
#[test]
fn history_holds_join_the_node_and_the_waiting_reasons() {
    use aq_history_caps::{Code, Hold, Scope};
    let mut i = input(vec![node(1, "a", WorkerHealth::Online, Some(1), 0, vec![])]);
    i.history = vec![
        Hold { code: Code::RateLimited, kind: "offers".into(), scope: Scope::Node(uid(1)), bytes: false, used: 30, limit: 30 },
        Hold { code: Code::CapReached, kind: "admission_artifacts".into(), scope: Scope::Project(uid(7)), bytes: false, used: 449, limit: 512 },
    ];
    let s = assemble(i).unwrap();
    let n = &s.machines[0].nodes[0];
    assert_eq!(codes(&n.holding), vec![HoldCode::HistoryRateLimited]);
    assert_eq!(n.holding[0].message, "Offer rate limit reached: 30 offers in the last hour (limit 30)");
    assert!(matches!(s.machines[0].state, MachineState::Holding));
    assert!(s.waiting.reasons.iter().any(|r| r.code == HoldCode::HistoryCapReached
        && r.message == format!("Project {}: live admission artifacts cap reached: 449 of 512", uid(7))));
}
```

- [ ] **Step 3: `src/dashboard_api/swarm.rs`.** After the `let owner_availability = aq_swarm::service::owner_availability(&mut tx).await?;` line, insert:

```rust
    // Spec 0008: history caps and the offer rate hold online nodes, visibly.
    let online: Vec<Uuid> = (cap.data.nodes.iter())
        .filter(|n| n.health == WorkerHealth::Online)
        .map(|n| n.node.id)
        .collect();
    let staging = (state.source_retention.as_ref())
        .map(|r| (r.config.max_staging_bytes, r.config.max_bundle_bytes));
    let history = aq_history_caps::swarm_holds(&mut tx, &online, now, staging).await?;
```
  Add `history,` as the last field of the `SwarmInput { ... }` literal. If `WorkerHealth` is not in scope, use `aq_dashboard_contract::WorkerHealth::Online`. The `?` conversion is the same one `aq_swarm::service::states(&mut tx, &ids).await?` uses two lines above.

- [ ] **Step 4: Schema and README.**

```sh
timeout 900 cargo run -p aq-server --example dashboard_contract
git diff --stat protocol/dashboard-read/v1/schema.json
```
Expected: only the `HoldCode` enum gains `"history_rate_limited"` and `"history_cap_reached"`.

  In `protocol/dashboard-read/v1/README.md`, in the swarm paragraph (the one beginning "A node's `holding` lists why placement would refuse it now"), add after the sentence that ends "...neither appears as a hold.":

  > `history_rate_limited` and `history_cap_reached` (spec 0008) say that the offer rate (30 an hour per node, 240 per project, 960 in all), the inventory report rate, or a live sealed-history cap holds new offers. On a node they apply to that node; in `waiting.reasons`, a message starting `Project <id>:` applies to one project and one starting `Server:` to all.

  Then run:

```sh
timeout 900 cargo run -p aq-server --example dashboard_contract -- --check
python3 protocol/dashboard-read/v1/check.py
```
Expected: both exit 0.

- [ ] **Step 5: Dashboard test.** Add to `tests/it_dashboard/dashboard_swarm.rs`:

```rust
#[tokio::test]
async fn swarm_shows_the_offer_rate_hold_on_the_node() {
    let now = common::profiled::NOW;
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    sqlx::query("UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1")
        .bind(f.node.node_id)
        .bind(now as i64)
        .execute(&pool)
        .await
        .unwrap();
    common::sealed::offers(&pool, f.node.node_id, f.selection.lineage.project_id, now as i64 - 1_000, 30).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let node = body["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["nodes"].as_array().unwrap())
        .find(|n| n["node"]["id"] == json!(f.node.node_id.to_string()))
        .unwrap();
    assert!(
        node["holding"].as_array().unwrap().contains(&json!({
            "code": "history_rate_limited",
            "message": "Offer rate limit reached: 30 offers in the last hour (limit 30)",
            "remedy": "Wait for the hour to pass; if this repeats, look for a task that fails in a loop"
        })),
        "{node}"
    );
}
```

- [ ] **Step 6: Run.**

```sh
timeout 900 cargo test -p aq-swarm -p aq-dashboard-contract -p aq-history-caps
timeout 900 cargo nextest run -p aq-server --test it_dashboard -E 'test(/^dashboard_swarm::/) | test(/^dashboard_read::/)'
```
Expected: all pass, including `history_holds_join_the_node_and_the_waiting_reasons` and `dashboard_swarm::swarm_shows_the_offer_rate_hold_on_the_node`. The `protocol/dashboard-read/v1/fixtures/swarm.json` fixture test still passes, because the fixture contains no history hold.

- [ ] **Step 7: Commit** `feat(swarm): show history caps and the offer rate as holds (spec 0008 A)`.

### Task 9: Docs, budget and full verification

- [ ] **Step 1:** Write `docs/implementation/0008-PA-server.md`. Follow the style of `docs/implementation/0007-P1-server.md`:
  - what was built: the migrations, the crate, the gate, the holds;
  - the limits table, copied from `sealed_history_limits()`;
  - the tests by name;
  - deviations from this plan, if any;
  - the deferred caps (spec §5.2), unchanged.

  Add a link line under the spec 0008 §12 table: `Phase A server: see docs/implementation/0008-PA-server.md.`.
- [ ] **Step 2:**

```sh
python3 scripts/build-budget.py --update
python3 scripts/build-budget.py
```
Expected: `OK`. The aq-server ratchet is lowered to the new count, which must be below 109185; the test binary count stays 81. If the lib count is above 109185, stop and report the count. Do not raise the ratchet.
- [ ] **Step 3: Full verification.**

```sh
timeout 900 cargo nextest run --workspace --locked --no-fail-fast
timeout 900 cargo test --workspace --doc --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
timeout 900 cargo run -p aq-server --example dashboard_contract -- --check
```
Expected: all green. A failure in a test this branch does not touch is rerun alone three times. If it passes, record it as a load flake, with its name and the run, in the implementation note.
- [ ] **Step 4: Commit** `docs: spec 0008 phase A server note; lower the lib ratchet`, then push `sealed-history-pa/server` and open the PR. Title: `Sealed history phase A: live-only caps, offer rate, visible holds (spec 0008)`. In the body:
  - each task;
  - the limits;
  - the deviations;
  - "Re-pin: dashboard-read (HoldCode +2)";
  - "Merges with aq-dashboard sealed-history-pa/dashboard".

## aq-dashboard tasks

### Task D1: Re-pin `dashboard-read` and accept the two codes

**Repository:** `/home/jkern/dev/aq-platform/aq-dashboard`, branch `sealed-history-pa/dashboard` from `origin/main`.

- [ ] **Step 1:**

```sh
python3 scripts/pin-contract.py /home/jkern/dev/aq-platform/<server worktree> dashboard-read <server PR head sha>
```
Expected: `contracts/pinned/dashboard-read/v1/schema.json` gains the two codes, and both `source.json` files record the sha.
- [ ] **Step 2:** In `src/api/wire/swarm.ts`, extend `HoldCode`: the last line becomes `| "no_machine_online" | "no_eligible_machine" | "no_free_slot" | "history_rate_limited" | "history_cap_reached";`. The swarm page renders `message` and `remedy` as given; no copy is added to the dashboard.
- [ ] **Step 3:**

```sh
pnpm typecheck && pnpm lint && pnpm test
```
Expected: all pass.
- [ ] **Step 4: Commit** `chore(contracts): re-pin dashboard-read for spec 0008 history holds`, then push and open the PR, linking the server PR.

## Merge and acceptance

### Task M1: Merge together

Merge the server PR and the dashboard PR one after the other, after both are green, with no other merge between them. After the server merge, re-run D1 Step 1 against the merge commit's sha if it differs from the pinned one, and push before merging the dashboard.

### Task A1: Live soak on aq-ms1 (stronger model)

1. Back up the stack's database (`~/.local/share/aq-ms1/backup-pre-0008-pa`, the way the existing `backup-pre-*` directories were made).
2. Record the counts per table:

```sh
for t in admission_artifacts intelligence_decisions profiled_source_exports admission_material_leases profiled_v2_proposals profiled_launch_registrations profiled_phase_terminal_capacity profiled_presence_operations profiled_launches; do echo "select '$t',count(*) from $t;"; done | ~/.local/share/aq-ms1/psql.sh
```
3. Rebuild the stack from origin/main with phase A merged (`~/.local/share/aq-ms1/up.sh`). Check that `SELECT value FROM meta WHERE key='state_format_version'` is `105`.
4. The first poll of a worker gets an offer, although the project still holds 512 admission artifacts.
5. Run a soak project of trivial tasks ("write a file named after the task and finish") through the workers until `profiled_launches` has at least 650 more rows than in step 2, and at least one node has taken more than 300. Throughout:
   - grep the server log for `native control request refused`: no line may name a quota, a capacity or `admission artifact`;
   - every `history_rate_limited` hold in `GET /api/v1/swarm` clears once the node's last-hour count falls below 30;
   - no attempt is left between offer and grant.
6. Deliberate hold, with the soak stopped:
   - seed 30 offers for one online node in the last hour (Task 7's SQL, on the live database; it adds relational-only rows, as the tests do). Do this only after the backup in step 1;
   - the Swarm page and `/api/v1/swarm` show the node `holding` with "Offer rate limit reached: 30 offers in the last hour (limit 30)";
   - the worker log shows empty polls, not 409s.
7. Write the evidence to `docs/implementation/0008-PA-acceptance.md`, committed on a docs branch with a PR:
   - the counts before and after per table;
   - the swarm JSON with the hold;
   - the log excerpts;
   - the number of offered attempts.

## Self-review notes

- **Spec coverage:**
  - §7.1 live counting: Tasks 2, 3, 5;
  - §7.2 rate limit: Tasks 2 (limits), 3 (inventory rate), 6 (fence), 7 (gate);
  - §7.3 gate: Task 7;
  - §7.4 visible holds: Task 8;
  - §7.5 migrations: Tasks 2, 3, 6;
  - §9 compatibility: Task 8 Step 4 and D1;
  - §11 acceptance: Task A1.
- **Deferred by the spec (§5.2), deliberately untouched:** registration history (`closure/history.rs:114-122`, 0028:50-58), presence heads, settlement evaluations, the review budget, intelligence receipts, legacy closures, legacy renew receipts, delegated operations.
- **Type consistency:**
  - `aq_history_caps::usage(conn, Kind, Option<Uuid>, u64) -> Usage` and `live_usage(..) -> Option<Usage>`;
  - `node_holds(conn, Uuid, u64, Option<(u64, u64)>)`, `project_holds(conn, Uuid, u64)`, `swarm_holds(conn, &[Uuid], u64, Option<(u64, u64)>)`;
  - `Hold { code, kind, scope, bytes, used, limit }`.

  These names are used unchanged in Tasks 5, 7 and 8.
