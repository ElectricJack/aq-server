# AQ v2 platform status

> **Before deploying `feat/internal-work-engine` to a live playbook namespace:**
> it vendors engine `aq-playbooks-5b86e0f` (0.2.0), which drains a namespace
> written by the engine the live stack runs (0173673). Measure the first open
> and drain on a copy of the live namespace under `[playbooks.engine_timeouts]`
> first ([note](implementation/playbook-authoring-B1-interim-vendor.md)).

**As of:** 2026-11-12. **Scope:** all six repositories (aq-server, aq-worker,
aq-dashboard, aq-playbooks, aq-git, aq-graph).

This is the one place that says where the build is against the specs. Update
it in the same pull request as any change that moves a row. The frozen
milestone plan ([`plans/0001-v2-milestones.md`](plans/0001-v2-milestones.md))
and the program plan
([`superpowers/plans/2026-11-02-v2-full-application-plan.md`](superpowers/plans/2026-11-02-v2-full-application-plan.md))
say what was planned; the supervision log
([`research/2026-11-02-v2-build-supervision-log.md`](research/2026-11-02-v2-build-supervision-log.md))
and the handoffs say what happened day by day. This file says what is true now.

Every row names its evidence level:

- **Live**: accepted on real machines, with an evidence document.
- **Tested**: merged and covered by automated tests, never run live.
- **Built**: code exists, not deployed or not reachable in the running stack.
- **Missing**: specified, no implementation.

The audit behind this revision read code and documents at the `origin/main`
revisions below. It did not query the live database or run anything.

| Repository | `origin/main` | Date |
| --- | --- | --- |
| aq-server | `8dca8043` | 2026-11-11 |
| aq-worker | `08a8608` | 2026-11-11 |
| aq-dashboard | `930ace5` | 2026-11-11 |
| aq-playbooks | `aaec4b2` | 2026-11-04 |
| aq-git | `180e614` | 2026-11-05 |
| aq-graph | `83ac59c` | 2026-11-05 |

## 1. Where we are

The local product works on one machine. On the `aq-ms1` stack (WSL2 desktop:
server, PostgreSQL, aq-graph, aq-git, supervisors, one native worker service) a
task can be created, offered to the worker, run by a real Claude agent in a
sandbox, settled and published. The dashboard reads and acts on live data on
all nine pages.

Three things are not true yet:

1. **No LAN worker has taken a queued task.** Other machines enroll, show in
   Swarm and run manual agents and host terminals. Every routed attempt so far
   ran on the desktop.
2. **The routing policy layer does not exist in the running system.** Placement
   is live; the routing-judgment playbook that should decide what each task
   needs is not (§3).
3. **Nothing is hosted.** The VPS, Cloudflare and cloud-worker milestones (MS2)
   have not started, and boat.dev workers depend on them (§6).

Since 2026-11-13 one platform rule governs every store that logs per-item
records: [records are rolling](architecture/0006-records-are-rolling.md). Old
records are deleted automatically and nothing refuses work because a store is
full. The specs are aligned with it; almost none of it is built, and many stores
still have lifetime caps (`research/2026-11-13-retention-audit.md`).

## 2. Component status

| Component | Status against its specs | Evidence |
| --- | --- | --- |
| aq-server: local execution (specs 0002, 0003, 0004) | **Live** on one machine. MS0 passed with 1 of 3 required runs (owner waiver). MS1 demoed 2026-11-06; its Windows-suspend drill was a dry run only. | [`implementation/MS0.md`](implementation/MS0.md), [`implementation/MS1.md`](implementation/MS1.md) |
| aq-server: agent channel (spec 0006) | Phases 1 to 6 **Live**. Phase 7 stopped before removing the old path; the boat channel is a design note. | aq-worker `docs/implementation/0006-P6.md`, `0006-P7.md` |
| aq-server: swarm (spec 0007) | Phase 1 **Live** (accepted on rerun 4, 2026-11-09). Phase 2 merged. Phase 3 controls are a draft (aq-server #131, aq-dashboard #105) already deployed on `aq-ms1`. Phases 4 (boat) and 5 (macOS) **Missing**. | aq-worker `docs/implementation/0007-P1.md` |
| aq-server: sealed-history retention (spec 0008) | Phase A merged; its offer and worker report rate limits removed 2026-11-13 (0124, on `feat/internal-work`). Phase B, restated 2026-11-13 under [architecture 0006](architecture/0006-records-are-rolling.md) (rolling summaries, task budget summaries, no history caps), is **Missing** and waits on owner answers. The A1 600-attempt soak has not run. | [`implementation/0008-PA-server.md`](implementation/0008-PA-server.md) |
| aq-server: terminals (specs 0009, 0010) | **Live** on the desktop and a laptop (2026-11-11). | [`handoff/2026-11-11-agents-terminal-workspace.md`](handoff/2026-11-11-agents-terminal-workspace.md) |
| aq-server: supervisors, manual agents, provider quota | **Live** (2026-11-10). | supervision log |
| aq-server: hosted deployment (spec 0001, MS2 to MS4) | **Missing**. `compose.yaml` describes it; nothing is provisioned. | [`operations.md`](operations.md) |
| aq-worker: framework and execution (specs 0001, 0002) | **Live** for the Claude sandbox path on one machine. Codex: one real run. Gemini and OpenCode: ignored live tests only. | aq-worker `docs/implementation/0007-P1.md`, `I0-first-real-run.md` |
| aq-worker: profiled tmux and terminals (spec 0003) | Session host, bridge and renderer **Tested**; manual sessions and host shells **Live**. Terminal control of queued attempts (W13) waits on an owner decision. | aq-worker `docs/implementation/0007-P1.md` |
| aq-worker: observation and suspension (spec 0004) | Hibernation **Built** over ports with no production adapters. Observation engine not adopted. | aq-worker `docs/implementation/WRK-15-integration.md` |
| aq-worker: boat backend | Adapter and VM-time accounting **Live** for conformance only. No agent launch path, so a boat worker cannot run an attempt. | aq-worker `docs/implementation/C7.md`, `0006-P7.md` |
| aq-dashboard (specs 0001 to 0004) | All nine destinations **Live** against the server. Task create, graph, comments, attempts, hold reasons, terminals and decisions are wired. Task-view parity (0004) is in phase 1. | aq-dashboard `docs/implementation/` |
| aq-git (spec 0001) | v0.2.0 released. **Live** in the stack: baked into the server and worker images, run as the `aq-git` compose service, and used by the worker for every source workspace. Performance and power-loss gates are not certified. | aq-git `docs/release-status.md`, `compose.yaml` |
| aq-graph (specs 0001 to 0004) | v0.1.1 released. **Live** in the stack as the authoritative task and knowledge store. The dev stack builds an older commit than the release digest. VPS and performance validation are open. | aq-graph `docs/implementation-status.md`, `compose.yaml` |
| aq-playbooks (specs 0001, 0002) | Engine **Tested** (model, compiler, runner, PostgreSQL store, CLI). In the server it is **Built**: the runner starts when `[playbooks]` is configured, but the live stack loads an inspection-only artifact with no triggers. | §3 and §4 |

## 3. Routing: what the design says and what runs

The design has two layers. They are easy to confuse because v1 had one.

| Layer | Specified in | Owner | Status |
| --- | --- | --- | --- |
| **Routing policy.** Read the permitted execution options, ask for a bounded judgment only when a fact is missing, and change the task's authorized requirements and preferences. | aq-playbooks `docs/specs/0001-aq-playbooks.md`: requirement P17, the routing paragraph in §6 "Initial AQ host API", the "Routing judgment" pilot row, test T25 | A playbook | **Missing in the running system.** The pilot runs against a fake host only. |
| **Placement.** Choose and authorize the actual execution from current provider availability, capacity and worker eligibility. | Same paragraph ("Actual placement remains AQ-owned"); aq-worker `docs/specs/0001-worker-framework.md` §8; [`architecture/0005`](architecture/0005-sandboxed-execution-and-deployment-topology.md) | The server | **Live.** A worker polls, the server picks a ready task, resolves a model and publishes an offer (`profiled_execution/producer/workflow.rs`). |

What stands in for the routing policy today is static configuration: each
project's intelligence policy, resolved at offer time. Nothing reads a task
and decides its class, risk or harness. The tasks that have run were routed
this way. This is a gap against the design, not a decision recorded anywhere.

v1 differed: its `default-assignment-routing` playbook both classified the
task and chose the route. In v2 the playbook cannot claim a worker or bypass
admission.

**Playbook authoring ([spec 0013](specs/0013-playbook-authoring.md)) is
Tested on `feat/internal-work`, not deployed** (2026-11-14). Built: engine
0.2.0 vendored; profile `host_v9` on host catalog 2.2.0 with `work.read`,
`work.route`, `work.unroutable` and `judgment.assess`; the server originates
`aq.work.route_needed`; a document saved in Settings is authored by an
internal-work agent into a playbook that the server seals again, checks
against its cases, installs and activates, with rollback and a sources API
for other documents; routing runs on that path and no server code decides a
route; a task that cannot be routed is raised once per cause. Not yet proven:
no real model has authored a playbook (every test hands the save path a
prepared outcome), the real-login checks, routing of every kind the document
names, and the 500-run scale check on an authored playbook (it still measures
the Host8 pilot, which is also the one open finding of
`scripts/check-authoring-inventory.sh`). Evidence, gaps and follow-ups:
[`implementation/0013-playbook-authoring.md`](implementation/0013-playbook-authoring.md).
The live check is the operator's
[`operations/0013-live-authoring-runbook.md`](operations/0013-live-authoring-runbook.md).
Items 2 to 6 below are built on that branch at this level; the table above
describes `main` and the running stack.

What the routing policy layer still needs, in dependency order:

1. A routing policy document in Settings, carried over from the v1 playbook's
   policy block and classifier guidance. **Done on `aq-ms1`** (2026-11-12,
   Intelligence structure revision 4). The field is saved source only; nothing
   reads it yet. The Settings field itself is on aq-server #131, not on `main`.
   The rest of this list is specified in
   [`specs/0011-routing-policy.md`](specs/0011-routing-policy.md) (draft).
2. aq-server re-vendored onto the current aq-playbooks runner. **Done on
   `feat/internal-work-engine`** (2026-11-13): it vendors `aq-playbooks-5b86e0f`,
   engine 0.2.0 (finished runs leave the State as history, rolling repeat
   protection, no namespace limits, the legacy drain, authoring tools)
   ([note](implementation/playbook-authoring-B1-interim-vendor.md)).
3. `judgment.request` bound to a provider. The server registers all twelve
   host commands but leaves `judgment.request` and `check.request` unbound, so
   they refuse (`playbooks/runtime.rs`). aq-server #64 is the open judgment
   provider change.
4. The routing playbook authored against the real host commands, installed and
   activated with a trigger on new work.
5. Placement honoring what the playbook writes on the task.
6. A typed routing outcome on the task and an attention item when a task
   cannot be placed. This is the v1 lesson W6 in
   [`research/2026-11-02-v1-hosting-retrospective.md`](research/2026-11-02-v1-hosting-retrospective.md):
   v1 routing runs failed silently and tasks sat READY.

## 4. Playbooks beyond routing

- The default coding pipeline (work, candidate, checks, review, optional
  decision, publish) exists in `playbooks/coding.rs` and installs through the
  API. It is not deployed.
- It cannot complete today: `check.request` is unbound (aq-server #63 is the
  open check executor), nothing outside playbooks writes the source receipt and
  review disposition it reads from work nodes, and its human decision has no
  HTTP route or dashboard action.
- Human gates do work end to end for the `human_review_v2` profile, through
  the Decisions page.
- aq-playbooks' scenario table is design dispositions, not executed scenarios:
  45 expressible with the initial engine, 25 that need a host capability.

## 5. First queued task on a LAN worker

This needs no playbook work. In order:

1. The worker's runtime must match the server's single qualified runtime
   (`qualified_runtimes` in the stack's `intelligence.toml`). The laptop's main
   worker was left on an older runtime on 2026-11-11 because an active manual
   agent blocked the restart.
2. The worker needs a fresh context-qualification measurement. A node enrolled
   without a provider login shows online and is refused every offer (spec 0007).
3. The task must be Active, and its project enabled. New tasks start as Draft
   by design: the owner reviews a task and its dependencies before making it
   Active, and only Active tasks are ready.
4. Owner availability on, machine not stopped or draining, live history caps
   clear, provider quota available. (The offer and worker report rate limits
   were removed on 2026-11-13, migration 0124.)

A task does not have to change a file. Since 2026-11-13 (on
`feat/internal-work`, not yet on `main`) an attempt that finishes successfully
without changing a file succeeds: its result is the agent's final message, and
nothing is published. The task reads "Completed, no changes". An agent that
ends with `Outcome: blocked` fails (rework reason `agent_blocked`); one that
claims changes that did not arrive fails too. Only a `Changes:` value that is
entirely a list of file paths (no notes, colons or other words) is such a
claim; any other wording is not. A handoff
successor that only verified its predecessor's edits still publishes them.

The recorded result is a summary of at most 500 bytes (the result report's
conclusion field). When the attempt has an agent channel, the driver also posts
the agent's whole final message (up to 16 KiB) as an agent comment on the task;
an attempt without a channel records the summary only.

## 6. Boat workers

"Boat" is the boat.dev VM backend, swarm phase 4. Still needed: phase 3
controls merged, a boat agent launch path, an in-VM channel proxy, a
path-restricted Cloudflare Access token, the server holding the boat API key,
a publicly reachable HTTPS server (MS2), and owner answers to the three open
questions in aq-worker `docs/implementation/0006-P7.md`.

## 7. Open defects and decisions

| Item | State |
| --- | --- |
| aq-server `main` CI | Red since the #130 merge (2026-11-11). The `tests` job also runs over its 10-minute budget. |
| M1 | Fixed 2026-11-08 by aq-server #121 (`a5966547`): an open dashboard no longer causes `nodes`-row lock timeouts (test `it_native` `scoped_presence_inventory`). A residual is fixed on `feat/internal-work` (2026-11-13, "fix(swarm): an owner machine action that hits a lock timeout reports busy, not unknown"), not yet on `main`: an owner Start, Stop, Drain or Revoke that hits its 1 s lock timeout changes nothing and now reports busy instead of an unknown outcome, and contention on the worker machine routes is a 503 with `Retry-After` instead of a 500. |
| M2 | Fixed on `feat/internal-work` (2026-11-13), not yet on `main`. Retry on a task held by its retry budget (or another hold) is refused with the hold, the charged and allowed attempts, and the remedy (cancel the task and create a new one). It used to report success and change nothing. |
| M3 | `release-sandbox` needs `--force`. |
| Completion policy | Fixed on `feat/internal-work` (2026-11-13), not yet on `main`. A task that changes no file succeeds, with nothing published. |
| Owner decisions | W13 (terminal input on queued attempts), 0008 phase B retention, the A1 soak, true resume, the three boat channel questions. |
| Forward-only schemas | 113, 114, 116 and 117. Rolling back the binary alone is unsafe. An older server binary also cannot read stored rejected dashboard operations that carry a `reason` (busy machine actions, refused retries, from `feat/internal-work`): their lookup or replay returns 503. |
| Follow-ups (`feat/internal-work`) | Raise the result report's conclusion limit (aq-admission 0.3.4) so the whole final message travels without an agent channel. A task-list filter for "Completed, no changes" (the list read carries no settlement evidence). Supervisors and the owner CLI cannot Retry a task (`task.retry` is a dashboard action only). The native agent startup text (aq-agent-result-local `startup.md`) does not yet ask for `Changes: none` and the `Outcome:` line: its digest is pinned by aq-server and the aq-session-host release. Re-pin the dashboard's copies of the dashboard-actions and results-feedback contracts after merge. Accepted residual: a `Changes:` value that is a single bare path (for example `README.md`) with nothing copied back fails the attempt with "the agent reported changes, but none were copied back", by design; at the next aq-session-host release the native startup text should ask for `Changes: none` when no file changed, and for the `Outcome: done` / `Outcome: blocked` line. |
| Stale pull requests | aq-server #3, #9, #53, #60, #63, #64; aq-worker #32. |

## 8. Next steps

1. Run the LAN worker test (§5) and fix what it finds.
2. Put the routing policy document in Settings (§3, item 1).
3. Finish the playbook work for the routing-judgment playbook (§3, items 2 to 6).
4. The coding pipeline (§4).
5. Hosted deployment, then boat workers (§6).
