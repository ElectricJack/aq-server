# F1a Central Session Owner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Persist the independent central session lifecycle while recording the physical supervisor host as blocked on `crisp-nexus-12`.

**Architecture:** A dormant PostgreSQL owner in `aq-central-authority` serializes each actor through a locked head row. It validates retained reducer states, commits permit transitions before returning them, retains every generation, and derives successor mail fences from that history. A required trusted verifier checks plan authorization and physical/mail evidence; no production implementation, route, daemon activation or physical authority claim is introduced before the subject-neutral host contract exists.

**Tech Stack:** Rust 1.96.1, sqlx 0.8, PostgreSQL, existing AQ-17 reducer.

**Spec:** Task `steady-harbor-87.1`, `docs/implementation/AQ-17-central-authority-core.md:47-55`, and operator instruction `msg-4ced906430644c58b79ae1ca9352aff3`.

## Global Constraints

Every task's requirements include these.

- **Builds:** each repository builds and tests alone. No sibling path dependencies, no submodules, no floating branches in release inputs. Rust uses the committed lockfile with `--locked` (ecosystem §4, E1).
- **Contracts:** a wire change publishes schemas and fixtures from the contract owner, and both endpoints consume a pinned version. The task is not done until valid, invalid, replay and compatibility cases pass on both sides (ecosystem §3).
- **Authority:** never add a second writable task lifecycle. aq-graph owns task state; AQ owns grants, attempts and dispositions (ecosystem §2).
- **Networking:** workers, runners and sandboxes initiate every connection. No worker or sandbox listener is reachable from outside, apart from loopback proxies (W2, R1, SBX7).
- **Hosting:** every real agent CLI runs in an attempt-owned tmux host under the guardian (H1), inside its own sandbox (SBX1). Central supervisors run in the supervisor host (DEP5).
- **Stop proof:** stopped-writer proof is required before resources are released or a replacement is started (X3, SBX4). A sandbox with unretained work is preserved, never destroyed (SBX5).
- **Secrets:** no secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions (P8, SBX9). Node, backend, Git remote and supervisor credentials never enter sandboxes (SBX17).
- **Isolation claims:** do not claim hostile-code isolation for shared-kernel containers (W6, SBX10).
- **Explicit refusals:** unsupported required capabilities refuse explicitly; nothing silently degrades (SBX13, SBX19).
- **Definition of done:**
  - `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` pass for the touched Rust workspaces. For aq-dashboard: `pnpm lint`, `pnpm typecheck` and `pnpm test`.
  - Evidence goes in `docs/implementation/<TASK-KEY>.md` in the owning repository, recording exact commands, environment, commit and remaining limits.
  - Changes are pushed through the project's integration policy.
  - Prior qualification statements are not reused for changed code.
- **Naming:** while v1 is live, no v2 binary, service or CLI may be installed under the names `aq` or `agent-queue`.
- **Review:** tasks marked `review: adversarial` require a cross-family adversarial review (`aq-reviews` recipe) before a passing close. They integrate by pull request; all other tasks integrate `direct`.
- **Named tests are the evidence:** each test deliverable names a specific test. It is met only when that test exists and its run reports it passed. A filter that matches zero tests is an unmet deliverable. The owner or supervisor reviews any `pass` close that declares `--deliverable-unmet`.
- **Docs steward:** only A-series and docs tasks edit `aq-server/docs/architecture/*` or any repository's `docs/specs/*`. Other tasks record design-document changes in their evidence and file a `docs` task, so parallel worktrees don't conflict on the canonical specs.

The operator-authorized partial submission remains blocked until the subject-neutral physical host, named acceptance tests and cross-family review are complete.

## Task 1: Durable lifecycle and successor history

**Files:**
- Create `crates/aq-central-authority/src/owner.rs` and `src/owner/history.rs`.
- Modify `crates/aq-central-authority/{Cargo.toml,src/lib.rs,README.md}` and `Cargo.lock`.
- Create `crates/aq-server/migrations/0045_central_sessions.sql`.
- Register migration and bump state format in `crates/aq-server/src/{store.rs,lib.rs}` and `Dockerfile`.
- Create `crates/aq-server/tests/central_sessions.rs`.
- Create `docs/implementation/F1a-evidence/test_cargo.py` for resource-gated commands.

**Interfaces:**
- Consumes `Run::{new,transition,successor}`, `CentralPlan`, `Clock`, `PermitData`, and verified host/mail artifact references.
- Produces `SessionOwner<V>` with `prepare`, `start`, `renew`, `report`, `inspect`, and `generation`.
- `EvidenceVerifier` authorizes plans and verifies evidence within the caller's trusted process. The owner takes no arbitrary HTTP caller input and supplies no permissive verifier.
- `Snapshot` contains revision and validated Run; `Receipt` contains Snapshot and reducer Decision. A refused transition may still persist expiry. DB/model failures are distinct from reducer refusals.

- [x] Write SQL integration tests first. The initial failing test imports the missing owner and demonstrates intended persistence:

```rust
let owner = SessionOwner::new(pool.clone(), FixtureVerifier);
let first = owner.prepare(plan.clone(), clock(1000)).await.unwrap();
assert_eq!(first.run.phase(), Phase::Prepared);
let started = owner.start(&plan.binding().unwrap(), launch(&plan), 120_000, clock(1000)).await.unwrap();
assert_eq!(started.snapshot.run.permit().unwrap().sequence, 1);
let reopened = SessionOwner::new(pool, FixtureVerifier);
assert_eq!(reopened.inspect(plan.subject.actor_id()).await.unwrap().unwrap(), started.snapshot);
```

- [x] Run `F1A_CHECK=owner aq test docs/implementation/F1a-evidence/test_cargo.py -q --timeout=1800`; confirm missing owner API fails compilation.
- [x] Implement SQL row locking, validated bounded JSON storage, actor/conversation/authority identity checks, and per-actor generation cap. Every successor increments once, preserves prior rows, and uses an internally derived `MailHistoryFence`.
- [x] Implement start/renew/report transaction semantics using the existing reducer:

```rust
let transition = snapshot.run.transition(&event, clock)?;
// Persist transition.state even if transition.decision refuses a late renewal:
// expiry is durable and cannot be rolled back by returning the refusal.
save_locked(&mut transaction, &transition.state).await?;
transaction.commit().await?;
```

- [x] Test concurrent initial preparation and successors, wrong binding/boot/regressed clock, permit limit and original deadline, late Running and renewal after cancellation/expiry, uncertain stop, verifier refusal, retained generation history, all-generation mail/cursor conflicts and cross-role operation reuse.
- [x] Run owner integration target and complete central-authority reducer tests; fix implementation defects without weakening expectations.
- [x] Commit the independent implementation.

## Task 2: Evidence and blocked submission

**Files:** `docs/implementation/F1a.md`, test adapter logs, and this plan.

- [x] Run formatting and targeted clippy for changed crates and test targets through the same resource gate. Full host acceptance is unavailable; do not name a persistence-only fixture as either required host test.
- [x] Record environment, exact commands, results, source base, implementation commit and remaining dependency in `F1a.md`.
- [x] Check `origin/dev`; if absent, record that the instructed final merge cannot run rather than silently substituting another branch.
- [ ] Commit and publish through `aq git push`.
- [ ] Close `--outcome fail --work-outcome blocked`, explicitly naming `crisp-nexus-12`, both unmet host tests and outstanding cross-family review. This follows the operator's direction and is not a passing submission.

## Deferred integration after crisp-nexus-12

The published subject-neutral host contract must replace the incompatible v1 task-bound launch input. Only then can F1a vendor the exact host/mapper packages, implement the trusted physical-evidence verifier, create the binary/image and protected credential adapter, qualify live adoption and stop-and-preserve, and run `supervisor_session_starts_under_central_permit` and `supervisor_restart_resumes_without_duplicate_session`. No substitute remote assignment or synthetic named-test pass is acceptable.

Publication and blocked-close receipts are recorded in the daemon task history after this evidence commit.
