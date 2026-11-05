# D9 — Default coding policy and playbook install/activation API (TDD plan)

Task: crisp-meadow-36.10. Goal: a default coding playbook built only from the
pinned Host7 (catalog 2.0.0) commands, plus an owner API to install, validate,
activate and roll back playbook artifacts without a daemon restart.

## Design

### Coding policy (`crates/aq-server/src/playbooks/coding.rs`)

`CodingPolicy { implementer_family, reviewer_family, checks, max_repairs }`
seals four artifacts with `Artifact::seal` (no compiler at runtime; the runner
verifies `validate_structure`, digest and fingerprint):

| artifact | binding | role |
|---|---|---|
| `aq.coding.work` | dependency | `work.ensure` (sync `completed` or async `accepted` → wait `aq.operation.updated`) → returns `accepted` / `changes_requested` / `escalated` |
| `aq.coding.check` | dependency | `check.request` (sync or async → wait `aq.check.receipted`) → returns `passed` / `failed` / `escalated` |
| `aq.coding.attempt` | dependency | work (implementation or repair) → `candidate.prepare` → every required check → review `work.ensure kind=review` with reviewer-family context → `candidate.publish` with check + review evidence → returns `published` / `rework{reference}` / `escalated` |
| `aq.coding` | project default | attempt 0 (implementation) then up to `max_repairs` repair attempts; each repair gets the failing evidence reference as its context |

Why child playbooks: bindings must be unique and there is no assignment
operation, so sync/async completion paths and the different failure origins
can only converge through a `Call` (Wait mode) whose outcome output is uniform.
Loops are unrolled (`max_repairs` is the bounded rework budget); no ForEach.

Refusals are explicit: equal implementer/reviewer families refuse at build;
every unexpected outcome routes to `escalated`.

### Artifacts API (`crates/aq-server/src/playbooks/artifacts.rs`)

- `install(op_id, bytes)` — `Artifact::from_canonical_bytes` (verified,
  immutable) → `Request::Install`.
- `validate(bytes)` — verify + registry compatibility (every command/event is
  exactly the pinned catalog contract).
- `activate(op_id, binding, digest)` / `rollback(op_id, binding)` — an
  activation ledger records `(binding, digest, previous)` per operation id;
  replaying an op id returns the recorded result (idempotent). Rollback
  re-activates the recorded previous artifact. Activation only changes the
  catalog for new admissions (runs pin their artifact at admission).
- Ledger: trait + in-memory impl + Postgres impl (`playbook_activations`
  migration).
- Owner routes under `/api/playbooks/artifacts...` and
  `/api/playbooks/bindings/{binding}/activate|rollback`.

## Tests (fast: MemoryStorage + fake Host7 peer, no DB)

1. `coding_policy_review_rejection_creates_rework_attempt` — review returns
   `changes_requested`; assert a `work.ensure kind=repair` intent is dispatched
   whose `context` is the review reference, then the run publishes.
2. `activation_switches_future_runs_only` — run 1 admitted on artifact A keeps
   A after B is activated; run 2 runs B; rollback restores A for run 3.

## Steps

1. Write test 1 (fails: no `coding` module). Implement `coding.rs`. Green.
2. Write test 2 (fails: no `artifacts` module). Implement ledger + API. Green.
3. Routes + migration + project-default wiring.
4. `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
   the two targeted tests.
5. `docs/implementation/D9.md`, follow-ups, squash, merge origin/main, push.

## Limits (documented, follow-ups)

- `decision.request` is not in the Host7 grant, so the optional human decision
  is omitted.
- Candidate source OID is not in the `candidate.prepare` output; check source
  comes from the run input.
- The real Host7 `work.ensure` returns `ensured` (no completion yet) → the
  policy escalates until D5–D8 emit `aq.operation.updated`.
- CLI is F7's.
