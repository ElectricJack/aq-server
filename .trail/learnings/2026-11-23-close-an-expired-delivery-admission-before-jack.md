---
title: Close an expired delivery admission before announcing the same result again
author: Jack
date: 2026-11-23
branch: fix/aq-task-liveness
cost: { minutes: 1820, tokens: 6967912 }
id: l-8bffe430
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-host/src/lib.rs
  - crates/aq-playbook-domain/src/host_operations.rs
  - crates/aq-server/src/playbooks/host_adapters.rs
  - crates/aq-playbook-host/tests/registered.rs
  - crates/aq-playbook-domain/src/delivery_store.rs
  - crates/aq-playbook-domain/src/host10.rs
  - crates/aq-server/src/playbooks/triggers.rs
  - crates/aq-server/tests/it_playbook/playbook_host9.rs
  - Cargo.lock
  - crates/aq-human-work/Cargo.toml
---

## Decided
Recover a lost async admission by recording its retained accepted handle in the engine. If the original deadline has passed and no admission exists, record expiry under the same namespace lock the original admission requires. Retire the expired playbook run so the existing result emitter can announce the same ticket result again.
AQ retains an already admitted owner retry while verifiable stop, source, effect, or lineage evidence for its exact predecessor is incomplete. Evidence and owner authority can arrive in either order. A pending approval preserves the original request and exposes who can resolve the current blocker and when AQ will check again.
In the code: task_execution::execute records the proof blocker through owner_retry::wait_on. The expiry sweep claims the durable intent with backoff, and owner_retry::released records the exact release in the replacement transaction.
Resume a publication hold through the original result after the forge again satisfies its lease, or already shows the exact result commit. Keep the hold and its original claim until the playbook decides the result. Re-emission uses the existing event and run ownership checks.
In the code: delivery_store::publish_conflict pins the lookup to the stopped attempt's result, repository and binding generation. The socket publish adapter recognizes a rejected target_moved receipt whose observed target already equals the requested result head.
When investigating an exhausted Code Health ticket, reconstruct its retained attempts and join each delivery decision to the CI head and operation that produced it. An owner retry can release the limit but the next worker still needs a working analyzer, required runtime, and a comparison against the same baseline as CI. If a retained result simply needs a new delivery check, use its exact task, result, and source binding.

## Tried and ruled out
Looking up more receipt types was insufficient. The publication regression still stalled because engine recovery applied Completed replies and skipped Accepted handles. For the requests that never reached admission, requesting another run immediately could race the original dispatch. The PostgreSQL regression interrupts admission at the operation insert and proves that both writes roll back, the deadline stays open until expiry, and exactly one fresh publication is admitted afterward.
Terminal rejection on a proof gap discarded a valid approval before the evidence could arrive. Treating a known proof wait as an unknown outcome also hid the distinction between waiting for evidence and a lost mutation acknowledgement. The response therefore stays pending, with the precise wait available on the Work surface.
The absent-branch lease remained a valid Git creation rule. Loosening it or treating every target_moved reply as success would conceal genuinely divergent heads. Live ticket b57bd9b1 had a retained rejection whose observed target and candidate both equalled a29e06278, and GitHub still showed that commit. The defect was the delivery interpretation and lack of a follow-up for the held result.
The surrounding GitHub job name was misleading. On this campaign, the job called fmt and clippy repeatedly failed at the library size ratchet before formatting ran. The source-only sandbox reported passing its absolute ceiling because it lacked the Git base for the relative limit. A separate Playwright retry appeared to require code edits, but GitHub recorded one failing and one passing browser workflow on the same unchanged head.

## Watch out
The engine records a rejected reply without making an already blocked run terminal. Resume retirement after a restart if the expiry rejection was already recorded. Keep the expired marker bound to the original intent and grant. A retained admission and an absent admission take different recovery paths.

In the code:
- crates/aq-playbook-host/src/lib.rs reconcile_delivery applies async handles and retires only the run whose exact admission was sealed.
- crates/aq-playbook-domain/src/host_operations.rs close_unadmitted_delivery seals the old intent without creating an external operation.
- crates/aq-server/tests/it_playbook/playbook_host9/delivery/decisions.rs exercises both missing and retained admission replies.
The approval binds the original predecessor, task version, and project binding. Never reuse it for a later attempt or infer it from an old journal row missing the original request. The replacement transaction still requires its existing proof and permission checks. A lost acknowledgement after release resumes by confirming that release.
A missing ref response can also mean lost access. Require a successful matching-ref read before concluding that a branch is absent. Later publication still uses its original source and target lease fences, and the existing pull request and check policy governs any merge.
A generic policy return includes lost worker handoffs and missing prerequisites. A current PR may also look healthy even though a later attempted result superseded the published head. Preserve the original evidence and re-evaluate the exact current result. Keep unconfirmed launch effects and unexplained backend timeouts distinct from ordinary code or CI failures.

Join execution_attempts and profiled_result_reports with delivery_decisions and playbook_host_operations using task, attempt, result, and expected head. Use GitHub job steps and logs for that SHA, and include both push and pull_request runs when checking duplicate check names. The ratchet is in scripts/build-budget.py.
