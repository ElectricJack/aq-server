---
title: Trace a recorded delivery hold through its notification receipt
author: Jack
date: 2026-11-21
branch: fix/held-task-recovery
id: l-49bb08f2
scope: repo
status: active
src: agent
touches:
  - crates/aq-playbook-domain/src/scheduling_notices.rs
  - crates/aq-delivery/src/git_grant.rs
  - crates/aq-server/src/profiled_execution/settlement/automatic.rs
---

## Decided
A stopped job can have a successful worker report, an exhausted delivery decision and an empty supervisor mailbox at the same time. Check the playbook transition receipts for the exact run and check the message receipt for the recorded result and reason. In one incident the hold committed, then the notification deadline elapsed before dispatch. A second result-ready event never arrived because the decision already existed. A delayed project scan using the same notice identity recovered that message exactly once.

## Tried and ruled out
A green PR from an earlier head and a worker's account of the push did not establish why integration stopped. The recorded checks operation for the exact result and published head proved the branch was behind after another PR merged. A normal owner retry still failed because replacement reconciliation accepted only a failed worker report. The successful report was valid. Its separate delivery failure justified reconstructing retention and effect evidence for that single retry.

## Watch out
A failed check for an earlier result cannot authorize a replacement. An unfinished merge can already have an external effect. Preserve the current claim while inspecting it, prove the source and operation ledger and let the ordinary retry path perform release. The project supervisor can inspect task.work and task.history; a global result lookup can be refused even for a task the supervisor owns.

In the code: scheduling_notices.rs shares the result/reason key with supervisor.notify; git_grant.rs reads the checks or merge receipt for the exact result and branch; settlement/automatic.rs revalidates the stopped claim before recording replacement facts.
