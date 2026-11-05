# C5a2 Material Lease Permit Binding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind each private material lease to the delegated permit issued for its attempt. A permit-authenticated material fetch then succeeds only for that permit.

**Architecture:** A new migration adds `admission_material_leases.permit_id`, guarded so it is set once and is consistent with the permit's attempt and node. `delegated_permits::handoff::issue_locked` binds the attempt's admitted, unbound leases to the preparation permit it creates, in the same accept transaction. `profiled_execution::material_api::fetch` refuses a delegated fetch with the uniform material refusal unless the locked lease row's `permit_id` and the immutable issued record (attempt, grant generation, sandbox) match the authenticated permit.

**Tech Stack:** Rust, axum, sqlx, PostgreSQL.

**Spec:** task calm-delta-18.13 (C5a2) description.

## Global Constraints
- Use the next free migration number (0076 now), with `UPDATE meta SET value='76' WHERE key='state_format_version';`. Renumber before pushing if main has moved.
- Keep the existing uniform refusal: `material_api::refused()` (409, "private material delivery is not currently authorized").
- Leave the node-credential path unchanged.
- Change no wire types, so no schema or fixture publication is needed.
- Acceptance: `cargo test --locked --workspace lease_bound_to_permit_refuses_other_attempt`.

## Review Focus
- A second permit for the same attempt with a different sandbox must be refused (the lease is bound to the first permit). This is in the test.
- A permit for another attempt on the same node and boot must be refused, whether it presents its own binding or the victim's lease id. This is in the test.
- After a permit is revoked, its bound lease must be refused. This is in the test.
- An unbound lease (legacy, or issued without a permit) must be refused to a permit but must still be served to the node credential. This is in the test.
- The binding must not be re-pointed to another permit; the migration guard refuses this. This is in the test, as a SQL UPDATE expected to fail.

---

### Task 1: Permit-bound material leases

**Files:**
- Create: `crates/aq-server/migrations/0076_material_lease_permit_binding.sql`
- Modify: `crates/aq-server/src/delegated_permits/handoff.rs` (`issue_locked`, after `save`)
- Modify: `crates/aq-server/src/profiled_execution/material_api.rs` (`fetch`, lease row recheck)
- Test: `crates/aq-server/tests/profiled_material.rs`

- [ ] **Step 1: Write the failing test** `lease_bound_to_permit_refuses_other_attempt`. Refactor `setup()` so that lease and attempt preparation is a helper. Build a second Preparing attempt B on the same node and boot, using the fixture mutation from `tests/dashboard_search.rs`. Install Preparation permits as follows: `storage::insert_locked`, plus a `delegated_permit_current` row holding a `runtime::Current` with `preparation: Some(PreparationAuthority{..})`. Assert:
  - P_A2 (attempt A, different sandbox_id) fetching lease A → 409. This fails before the change.
  - P_B with binding A → 403, and P_B with binding B and lease A → 409.
  - The unbound lease B, fetched with P_B → 409; the node credential fetching lease B → 200.
  - P_A (bound) → 200 with SECRET.
  - The UPDATE that re-points lease A to P_A2 fails.
  - After `UPDATE delegated_permits SET revoked=true` for P_A, P_A → refused.
- [ ] **Step 2: Run** `aq test -- -p aq-server --test profiled_material lease_bound_to_permit_refuses_other_attempt`. Expected: FAIL, because the `permit_id` column is missing and P_A2 gets 200.
- [ ] **Step 3: Write the migration.** Add `permit_id UUID REFERENCES delegated_permits(permit_id)` and trigger `admission_material_permit_guard`, BEFORE INSERT OR UPDATE. The trigger rejects:
  - a change to a non-null `permit_id`;
  - a non-null `permit_id` whose permit has a different `attempt_id`/`node_id` from `admitted_attempt_id`/`node_id`;
  - binding a revoked permit.

  Add an index on `permit_id`.
- [ ] **Step 4: Bind in `issue_locked`.** Run `UPDATE admission_material_leases SET permit_id=$1 WHERE admitted_attempt_id=$2 AND node_id=$3 AND state='admitted' AND permit_id IS NULL AND lease_id IN (SELECT lease_id FROM profiled_launch_materials WHERE attempt_id=$2)`.
- [ ] **Step 5: Check in `fetch`.** When `auth.delegated()` is `Some`, the lease row recheck LEFT JOINs `delegated_permits`. It requires: `permit_id = record.permit_id`; the permit is not revoked; `permit.attempt_id = admitted_attempt_id = record.binding.attempt_id`; and the record's binding `sandbox_id`/`grant_generation` equal the authenticated binding. Otherwise it returns `refused()`.
- [ ] **Step 6: Run** the acceptance test plus `profiled_material`, `delegated_permit_runtime`, `native_preparation`. Expected: PASS.
- [ ] **Step 7: Commit.**
