# AQ v2 full application: program implementation plan

> **For agentic workers:** This is a program plan. The per-task contracts live in [`2026-11-02-v2-full-application-tasks.yaml`](2026-11-02-v2-full-application-tasks.yaml), which is imported into AQ v1 as one tracked task each.
>
> When you hold one of these tasks, your first step is to write that task's own TDD implementation plan with `superpowers:writing-plans`. Save it as `docs/superpowers/plans/<date>-<task-key>-<slug>.md` in the owning repository, with exact files, signatures and tests. Then execute it with `superpowers:executing-plans` or `superpowers:subagent-driven-development`. Steps use checkbox (`- [ ]`) syntax.

**Goal:** finish AQ v2 to the owner's definition of done:

- the dashboard is fully working;
- the server is hosted on a Bluehost VPS behind Cloudflare;
- cloud (boat.dev) and local workers connect to it and run sandboxed agents;
- the owner can talk to the supervisor and enqueue work as in v1;
- it all just works.

**Architecture:** these stay as designed:

- the central daemon (aq-server) with embedded playbooks;
- aq-graph and aq-git as separately owned components;
- workers that connect outbound.

The plan adds per-attempt sandboxes from shared templates. The work is organized as ten workstreams (epics) whose tasks are ordered by explicit `needs` edges, not by phases, so that independent tracks run in parallel. Four milestone gate tasks prove user-visible outcomes on the real topology.

**Tech stack:**

- Rust workspaces with pinned `rust-toolchain.toml`, built with `--locked`;
- PostgreSQL;
- Docker Compose;
- Cloudflare Tunnel/Access;
- React/TypeScript/Vite (dashboard);
- the tmux session host;
- boat.dev API; Cloudflare Containers plus a gateway Worker;
- GHCR.

**Spec:** the canonical designs are:

- [ecosystem and releases](../../architecture/0001-ecosystem-and-releases.md)
- [execution and recovery](../../architecture/0002-execution-and-recovery.md)
- [profiles, harnesses and terminals](../../architecture/0003-agent-profiles-harnesses-and-terminals.md)
- [session operations, waits and notifications](../../architecture/0004-session-operations-waits-and-notifications.md)
- [sandboxed execution and deployment topology](../../architecture/0005-sandboxed-execution-and-deployment-topology.md)

Daemon specs are in [`../../specs/`](../../specs/). Component specs are in `aq-worker`, `aq-dashboard`, `aq-graph`, `aq-git` and `aq-playbooks` under `docs/specs/`. The starting point is the gap analysis of 2026-11-02 (G1–G10), reproduced in the appendix.

---

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

## Review Focus

These are the five conditions a user is most likely to hit that no task's ordinary tests exercise. Each one is pinned by a named test in the task listed.

1. **Desktop sleep or WSL shutdown mid-attempt.** Expected: on wake, the local worker reconciles, re-adopts or preserves each sandbox, and no work is lost. The attempt either continues within its deadline or is retried in a new sandbox. Daemon-pause test `wsl_suspend_resume_reconciles_sandboxes` in **C2**. A real Windows-suspend drill is part of the MS1 demo, because WSL2 clock and network behaviour across sleep can't be unit-tested.
2. **boat API outage, rate limit or account sandbox limit.** Expected: offers to the cloud worker are refused with a typed reason, work flows to the local worker, and no paid VM is left running. Test `boat_unavailable_refuses_offers_without_orphans` in **C7**.
3. **Expired or revoked subscription login.** Expected: the attempt fails fast with a credential-specific reason, the owner is notified once, and the task is not retried in a loop that burns budget. Worker-side test `expired_subscription_login_fails_fast_with_credential_reason` in **C6b**; notify-once test `credential_invalid_notifies_once_per_episode` in **F4**.
4. **VPS reboot or server upgrade during running attempts.** Expected: runners keep their journals and resume renewal when the server returns within the renewal window, otherwise stop and preserve. No duplicate effects. Test `server_restart_mid_attempt_resumes_or_preserves` in **B2**.
5. **An agent closes "pass" with failing tests or unpushed work.** Expected: the attempt's result disposition and the default completion policy refuse completion, dependent work is not released, and the task is reopened with the failing evidence. Test `false_pass_result_does_not_complete_task` in **D3**.

---

## 1. Milestones

| Gate | Outcome the owner can see | Required tasks |
| --- | --- | --- |
| **MS0: first real run** | Codex CLI, signed in with the owner's ChatGPT login (no API key), completes a real graph task three times. It runs in a plain container on the desktop, on today's native runner, against a local full stack. The problems found are filed as tasks before the larger build starts. | E0, I0 |
| **MS1: real agent, sandboxed, local** | On the local stack, a graph task runs Codex, signed in with the owner's ChatGPT login, in a `docker-local` sandbox on the desktop worker. The work flow is: edit code, run checks, publish the change, then the task completes automatically and the next task starts. Cancel, retry, worker/server restart and a Windows-suspend drill behave safely. | MS0, A3a, A3b, B1–B7, C1, C2, C3a–c, C4, C5a1–2, C5b, C6a, C6b, D0, D1a–b, D3, I4a |
| **MS2: hosted** | The full stack runs on the Bluehost VPS behind Cloudflare. Two workers are enrolled: the desktop worker, **running in a container on WSL2**, and the VPS cloud worker running `docker-local` sandboxes on the VPS. The MS1 scenario passes on both with the owner's ChatGPT login, including a login refresh written back. A VPS reboot recovers, and off-host backups are configured. | MS1, A1, A4, I4b, E1a–c, E2, E3, E5, E6a, E6b |
| **MS3: daily driver** | From the dashboard or the `aqv2` CLI the owner can talk to the supervisor and have it enqueue work. Agents message the supervisor, ask the owner questions and get answers. Discord notifies. The core dashboard actions work. **boat.dev** sandboxes join as a cloud backend, and **Claude Code** works. This is the v1-equivalent working loop on v2. | MS2, C7, D2, D4–D9, F1a–b, F2–F5, F7, G1a–b, G2–G8, I1 |
| **MS4: complete product** | Gemini CLI and OpenCode work. Session health, accounting, stalls, waits and hibernation run. Full dashboard visual acceptance passes. The deployed system is measured at about 10 concurrent attempts, and backup/restore is drilled. The acceptance matrix maps every requirement to evidence or an owner-approved deferral. | MS3 and the remaining active tasks |

The owner's definition of "finished" is MS3 plus MS4's dashboard and reliability items. MS0 exists because no real-provider run has ever completed in v2. It finds the real problems before about 25 tasks of scaffolding begin.

### Deferred backlog (owner decision, 2026-11-02)

These stay in the task file with `deferred: true`. The importer skips them, and the MS4 acceptance matrix records them as owner-approved deferrals.

| Key | Deferred item |
| --- | --- |
| C8 | Cloudflare Containers backend and gateway Worker. The design stays in 0005 §5.3; build it after MS4. |
| E4, J6 | Product manifest format 2, multi-component updater orchestration and upgrade drills. Until then: pinned digests in Compose, plus the format-1 updater. |
| E7 | Cloudflare Access token automation. Tokens are created by hand, two per worker. |
| J1, J2 | Hard aq-git and aq-graph latency budgets. They are measured in J3 instead. |
| F6, F8 | Daily digest; inbound Discord. |
| (H5 scope) | boat warm-start hibernation. Hibernation uses checkpoint-to-retention only. |
| (spec) | The 500-agent / 50-worker profile (0002 §10) and the 10,000-continuation profile (0004 §6), replaced by J3's measurement at about 10 concurrent attempts. |

## 2. Workstreams (v1 epics)

| Epic | Scope | Tasks |
| --- | --- | --- |
| **A. Tracking and provenance** | Plan reconciliation, requirement namespaces, aq-git re-pin and release, image hygiene; docs steward | A1, A3a, A3b, A4 |
| **B. Execution core** | Native contract ownership and delegated permits, cancellation, retry and fair scheduling, the unified node controller, the in-sandbox runner, agent AQ access | B1–B7 |
| **C. Sandboxes and templates** | Backend trait and conformance, `docker-local`, the template pipeline, environment/template catalog and placement, provider credential vault, runner credential handling, `boat`, `cloudflare-containers` | C1–C8 |
| **D. Work loop and policy** | Code delivery, repository onboarding, automatic settlement and outbox, catalog reconciliation, the nine missing host adapters, triggers, the default coding policy, playbook persistence performance, pilots | D1–D11 |
| **E. Deployment and release** | Production Compose stack, graph image, dashboard release archive, product manifest format 2 and updater, VPS and Cloudflare provisioning, worker installers, Access token lifecycle | E1–E7 |
| **F. Supervisor and communication** | Hosted supervisor, enqueue-work commands, agent messaging, automatic notifications, agent questions, escalation and digest, v2 human CLI, inbound Discord | F1–F8 |
| **G. Dashboard and human API** | Action union, typed commands, Work/Knowledge writes, Overview and search, Results feedback and media, supervisor panel, playbook actions, Agents operations views, visual/accessibility acceptance, bundle compatibility | G1–G12 |
| **H. Session operations** | Observation adoption, accounting, stall ladder, waits, hibernation and wake, terminal renderer | H1–H7 |
| **I. Harnesses** | Codex real-provider qualification, then Claude Code, Gemini CLI, OpenCode | I1–I4 |
| **J. Hardening and acceptance** | aq-git and aq-graph performance and durability, product capacity, security review, backup/restore, upgrade and compatibility drills, repository profile | J1–J7 |

**Critical path (after the parallelization review):** MS1 ≈ 9 sessions via A3a → C3a → C3b → C3c → C6a → I4a; MS2 ≈ 13 via B1 → B5a → B5b → E1c → E5 → E6b → I4b; MS3 ≈ 15. The MS0 release gate was removed, so MS0 runs in parallel. Earlier breakdown, kept for reference: B1 → B6a → B6b → B7. C1 → B5a → B5b. C3a → C3b → C3c. (C5a, B6b, C3c) → C6a → I4a. A3a → A3b → D1a → D1b. All of these join at MS1, and B3, B4, D3 and C4 run in parallel.

**Parallel from day one:** E0, A1, A3a, A4, B1, B3, B4, C1, C3a, D3, D4, D10, E2, E3 and G1a have no prerequisites. Under the initial 3-slot limit, start E0 → I0 first.

## 3. How old plan IDs map

The 2026-10-30 graph ([`0001-v2-milestones.json`](../../plans/0001-v2-milestones.json)) is frozen by **A1** as the historical baseline. Each new task names the old IDs it continues in its `continues:` field. Completed old tasks remain valid evidence only for the code they qualified. G1 found that WRK-02 to WRK-07 qualified the synthetic runtime, not the native path, so B5 and B6 must re-earn those guarantees.

---

## 4. Running this plan in AQ v1

### 4.1 Why agents stopped, and what v1 changes

Agents driving the old plan stopped after finishing a feature because nothing told them a next unit of work existed. v1 changes that in five ways:

- **Each feature is a task.** Every task in the task file is one v1 task, sized for one session. Every close gets a fresh session (`swarm.fresh_context_per_task`). The queue, not the agent, decides what runs next.
- **Readiness is a graph fact.** Dependents become ready when their prerequisites close `pass` and deliver. No one has to remember to continue.
- **Closing is enforced.** Each task carries:
  - acceptance criteria in its description;
  - `--deliverable` entries (a test command and, for design or contract work, a review);
  - the standard subtask checklist (section 4.4).

  A `pass` close with unmet deliverables or open subtasks is refused.
- **Emergent work is filed, not absorbed or abandoned.** An agent that discovers out-of-scope work files a child or root task with `--reason` instead of stopping or widening its scope.
- **Stalls are visible.** Stale `BLOCKED`/`PAUSED` detection, `aq task explain` and the supervisor's stale messages surface stuck work to the owner.

### 4.2 One-time setup (operator, before import)

1. **Start and check v1.** Run `aq start`, then `aq status` and `aq doctor`. The daemon was not running on 2026-11-02, even though the crontab watchdog installed by `aq service install` is present. Find out why it didn't restart the daemon. The v1 worker that runs C3c also needs Docker and a GHCR token.
2. **Clean the repositories.**
   - Commit the four dirty aq-playbooks files, including the uncommitted PB-01 evidence that PB-01's "complete" status cites.
   - Commit the two aq-graph documentation files.
   - Commit or discard the untracked `aq-server/docs/research/2026-10-30-v1-parity-gap-analysis.md`.
   - Commit the 2026-11-02 design changes (0005 and the five referencing documents) and this plan.
   - Push all six repositories.
3. **Create the project.** `aq project create --name aq-platform --from-policy default --max-concurrent-agents 3`. The project ID is derived from the name (lowercased, spaces become hyphens), so a display name like "AQ Platform (v2)" would produce `aq-platform-(v2)`. Start at 3 concurrent agents: Rust suites take about 3 minutes each, and target directories are several GB. Raise the limit after watching the first wave.
4. **Define workspace kinds** in `~/.agent-queue/vault/projects/aq-platform/workspace-kinds/`:
   - Six writable git kinds, one per repository: `aq-server-repo`, `aq-worker-repo`, `aq-dashboard-repo`, `aq-graph-repo`, `aq-git-repo`, `aq-playbooks-repo`. Each has `writable: true`, `lockable: true`, `is_git_repo: true`, `repo_url: https://github.com/ElectricJack/<repo>.git`, `default_lock_mode: branch_isolated` and `mode: worktree`.
   - `worktree_setup` for the Rust repositories gives each worktree slot its own `CARGO_TARGET_DIR`; a shared one would serialize builds on Cargo's lock. It enables `sccache` for cross-slot reuse. For aq-dashboard it runs `pnpm install --frozen-lockfile`.
   - One read-only kind, `platform-ref`: `readonly-dir`, `auto_attach: true`, linked at `/home/jkern/dev/aq-platform`. Agents can then read sibling specifications at `main`, which the cross-repository links assume.
5. **Register workspaces.** Agents must not use the owner's dev checkouts as the worktree base. Clone each repository fresh into a dedicated directory, `/home/jkern/dev/aq-platform-agents/<repo>`, and link that: `aq project add-workspace --project-id aq-platform --source link --kind-id <kind> --path /home/jkern/dev/aq-platform-agents/<repo>`. Add `platform-ref`, linked read-only at the owner's `/home/jkern/dev/aq-platform`.
6. **Choose integration mode.** The project default is `direct`. Tasks marked `review: adversarial` pass `--integration-mode pull_request`. That keeps dozens of PRs from waiting on the owner, which would look exactly like "agents stopped". Pilot the policy on E0 and A4.
7. **Routing.** Don't pin profiles; v1's router chooses. The task file gives a `type` and an `intelligence_class` hint per task. Contract, design and security tasks get `deep-high`; most implementation gets `standard-high`.

### 4.3 Import

A small importer, `aq-server/scripts/import-v1-plan.py`, is written as the first operator action after approval and is itself committed. A whole graph can't carry per-node workspace requirements, so the importer works like this:

1. **Validate first.** `--dry-run` checks: every `needs` key exists, the graph is acyclic, no active task depends on a deferred one, every repository maps to a kind, and every deliverable is well-formed. The importer is written and its dry-run shown to the owner **before** any task is created.
2. **Create the epic containers.** One per workstream: `aq task create --container`.
3. **Create each non-deferred task** with `aq task create --project "$AQ_PROJECT_ID" --parent <epic> --requires-kind <repo>-repo --type <type> --intelligence-class <class> [--integration-mode pull_request] --deliverable <json>…`. Gate tasks use `aq task edit --verification-type human`. The description is assembled from `description`, `acceptance`, `spec` and `continues`, plus the standard footer (section 4.4).
4. **Add the standard subtasks.** `aq task subtask-add <id> --title …` for each row in section 4.4.
5. **Add the edges.** `aq task add-dependency --task-id <id> --depends-on <upstream> --dep-type blocks` for every `needs` edge.
6. **Record the mapping and make it rerunnable.** The importer writes `aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-v1-ids.json`, mapping task key → v1 id. A rerun skips keys already in the mapping, so the import is idempotent. Edges are added only after every node exists.
7. **Release in waves.** Create all tasks, then pause the project (`aq project pause`). Resume once the pilot (A1, A4) has closed cleanly.

If `aq task batch-propose` is found to accept per-task workspace requirements, use it instead, so that the import is one atomic, human-gated batch.

### 4.4 Standard footer and subtasks for every imported task

Description footer, appended verbatim:

> **Complete this whole task in this session.** Don't stop after one sub-step. Read the cited specifications in `platform-ref` first.
>
> Write this task's TDD implementation plan (`superpowers:writing-plans`) in the owning repository before writing code. Then implement it test-first.
>
> If you find work outside this task's scope, file it with `aq task create --reason …`, then continue with this task. Don't edit `aq-server/docs/architecture/*` or `docs/specs/*` unless this is an A-series or docs task. Record proposed design-doc changes in your evidence and file a `docs` task.
>
> Each named-test deliverable must exist and pass. A filter that matches zero tests is not met.
>
> If a decision only the owner can make blocks you, report it with `aq message send --to user:dashboard --project "$AQ_PROJECT_ID" --body "Blocked on <KEY> (<task id>): <the decision needed>"`, record it as a task comment, and wait for the reply. Don't guess.
>
> Close `pass` only when every acceptance item is met with evidence in `docs/implementation/<TASK-KEY>.md`. Otherwise close `fail` with what remains.

Subtasks:

1. Read the cited specs, the prior evidence for the `continues:` tasks and the current code. Record the findings as a task comment.
2. Write and commit the task's implementation plan.
3. Implement test-first. The named tests in the acceptance criteria must exist and pass.
4. Run the full definition-of-done checks for every touched repository.
5. Write the evidence document: commands, environment, commit, limits.
6. Get an adversarial review if the task file marks one, and resolve the findings.
7. Commit, push and close with `--changes`, `--verification` and `--test`.

### 4.5 Milestone gates

MS1–MS4 are tasks of type `operator` with `verification_type: human`, owned by aq-server. Their agent assembles the scenario, runs it and writes the evidence. The owner then approves or reopens the gate, so milestone completion is a human decision. Each gate also produces a short demo script the owner can rerun.

---

## 5. Standing up the deployment

This is the order in which tasks bring the topology up. Requirements are in 0005 §3 and §9 and in the [infrastructure foundation](../../specs/0001-infrastructure-foundation.md).

1. **Local full stack (E0, MS0).** `compose.dev.yaml` runs postgres, aq-server, aq-graph and supervised aq-git with dev auth on WSL2. I0 then runs real Codex in a plain container against it.
2. **Local sandboxes (MS1).**
   - Install the rootless sandbox daemon on WSL2 (C2).
   - Build and publish the `rust` and `node` templates (C3a–c).
   - Run the node controller (B5) with the `docker-local` backend.
   - Run the MS1 scenario.
3. **VPS (E5).**
   - **Purchase:** size the Bluehost plan from measured MS1 memory. The VPS also hosts agent sandboxes for the cloud worker, so plan for the stack plus two concurrent Rust-building sandboxes. That is roughly 8 vCPU / 16 GB, to be confirmed. Check Bluehost's sustained-CPU terms before a prepaid term.
   - **Host:** provision the OS, firewall, Docker/Compose and the host updater.
   - **Cloudflare:** create the Tunnel and two hostnames:
     - human, behind Access with owner login;
     - machine, behind Access service tokens.
   - **Service tokens:** create them by hand in the Cloudflare dashboard, two per worker (node and runner).
4. **Server stack on the VPS (E1a–c).**
   - Install the pinned stack: server with the built-in dashboard, graph, aq-git, cloudflared, supervisor host and `aq-worker-cloud` (docker-local, with its own rootless sandbox daemon and per-service resource limits).
   - Verify Access origin isolation and reboot recovery.
   - Configure off-host pg_dump and vault backups now. Daily work lives here from MS3.
5. **Local worker in a container (E6a).** The desktop worker runs as a container on WSL2 with its rootless sandbox daemon. It points at the VPS machine hostname with its node and runner tokens, survives Windows restarts, and keeps the WSL VM alive.
6. **Credentials (C5a–b).**
   - **No API keys are required.** Agents use the owner's CLI subscription logins: the Codex ChatGPT login and the Claude login.
   - **Import the existing login files into the vault:** `~/.codex/auth.json` and `~/.claude/.credentials.json`. Device-auth enrollment is an alternative.
   - **Refreshes are written back.** v1's agents on the desktop share the same login files, so a refreshed login must not strand them.
   - **Logins are allowed on every backend.** This is a personal deployment, and cloud compute supplements the desktop.
   - **API-key profiles stay supported but optional.**
7. **Run MS2.** Run the MS1 scenario on both workers on the owner's ChatGPT login, then reboot the VPS mid-attempt.
8. **boat (C7, MS3).**
   - **Account:** create a dedicated boat organization for attempt sandboxes, plus a separate scope for template builds.
   - **Scoped key:** issue a key with lifecycle, command and file permissions.
   - **Templates:** materialize the template snapshots in boat.
   - **Worker:** enroll a second VPS worker node, `aq-worker-boat`, with the boat backend and spend and concurrency ceilings. `aq-worker-cloud` stays on docker-local.

## 6. Task index

This index is generated from the task file and covers active tasks only; deferred tasks are listed in section 1. The full self-contained contracts are in the task file.

| Key | Repo | Title | Needs |
| --- | --- | --- | --- |
| A1 | aq-server | Freeze the 2026-10-30 plan, namespace requirement IDs, add missing requirement sources | none |
| A3a | aq-git | Release aq-git 0.2.0 from HEAD with a verifiable artifact and Git 2.56 qualification | none |
| A3b | aq-server | Re-pin the shipped aq-git to the 0.2.0 release | A3a |
| A4 | aq-server | Image and CI hygiene: digest-pinned bases, state-format default, stale vendor trees, check scripts in CI | none |
| E0 | aq-server | Local full-stack Compose for development: graph, supervised aq-git, dev auth | none |
| I0 | aq-worker | MS0: one real Codex run on the owner's ChatGPT login in a plain Docker container on the existing native path | E0 |
| B1 | aq-worker | Own the native execution contract and add delegated runner permits | none |
| B2 | aq-server | Issue, scope, revoke and re-adopt delegated permits | B1, B3 |
| B3 | aq-server | Native cancellation end to end | none |
| B4 | aq-server | Retry/replacement attempts, fair multi-project scheduling and moving-ref sources | B3, I0 |
| B5a | aq-worker | Node controller core: enrollment, boot incarnation, journal-before-backend, capacity | B1, C1 |
| B5b | aq-worker | Node controller recovery: reconcile, re-adopt, preserve-before-destroy, sweeps, cancellation | B5a |
| B6a1 | aq-worker | Runner core: guardian and tmux launch, stop-and-preserve, handoff-before-done | none |
| B6a2 | aq-worker | Runner permit authentication on every runner route | B1, B6a1 |
| B6b | aq-worker | Runner OS users and loopback proxies (Responses; AQ API with Access header) | B6a1 |
| B6c | aq-worker | Runner hydration from retention and per-sandbox terminal/observation channels | B6a2 |
| B7 | aq-worker | Scoped AQ access for agents inside sandboxes | B6b, B2, C3a |
| C1 | aq-worker | Placement backend trait, journaled lifecycle and conformance suite | none |
| C2 | aq-worker | `docker-local` backend with a dedicated rootless sandbox daemon | C1 |
| C3a | aq-worker | aq-sandbox-os base image and aq-sandbox-runtime bundle | A3a |
| C3b | aq-worker | Template assembly, recipe format, finalize check, credential scan, allowlist and provenance | C3a |
| C3c | aq-worker | Publish base and runtime to GHCR and seed the rust and node templates | C3b |
| C4 | aq-server | Environment-to-template catalog, backend capabilities and placement preference | C3c, B1, B4 |
| C5a1 | aq-server | Provider credential profiles and per-credential backend policy | none |
| C5a2 | aq-server | Material leases bound to delegated permits | B2, C5a1 |
| C5b | aq-server | Subscription-login refresh write-back with refresh-lineage fencing; Codex device-auth enrollment | C5a1 |
| C6a | aq-worker | Runner Codex configuration for API keys and harness confinement modes | C5a1, B6b, C3c |
| C6b | aq-worker | Runner subscription-login install, refresh write-back and wipe | C6a, C5b |
| C7 | aq-worker | `boat` backend | C1, C3c, B5b |
| D0 | aq-git | Authenticated GitHub remotes in aq-git for fetch and publish | none |
| D1a | aq-server | Candidate creation and conditional publication through aq-git with durable intents | A3b |
| D1b | aq-server | Per-project publication policy and ambiguous-publication reconciliation | D1a, D0 |
| D2 | aq-server | Repository onboarding and central Git remote credentials | D1b, D0 |
| D3 | aq-server | Automatic result settlement and the confirmed-event outbox consumer | B3, I0 |
| D4 | aq-playbooks | Reconcile the synthetic and production host catalogs | none |
| D5 | aq-server | Host adapters: context.read, work.ensure, work.change, operation.cancel | D3, D4 |
| D6 | aq-server | Host adapters: judgment.request, check.request | D5, B5b, C4 |
| D7 | aq-server | Host adapters: candidate.prepare, candidate.publish | D1b, D5, D6 |
| D8 | aq-server | findings.reconcile, schedules and domain-event triggers | D3, D5 |
| D9 | aq-server | Default coding policy and playbook install/activation API | D5, D6, D7, D8, B4, G1a |
| D10 | aq-playbooks | Bounded transition persistence (PB-04) | none |
| D11 | aq-server | Routing, watcher and reporting pilots on real adapters | D9, D10 |
| E1a | aq-server | Production Compose: graph, supervised aq-git, cloudflared, digest-pinned images | A3b, E2, E0 |
| E1b | aq-server | Dashboard release archive built into the server image, with asset caching | E1a, E3 |
| E1c | aq-server | Supervisor host and cloud worker services in production Compose | E1a, F1a, B5b, C2 |
| E2 | aq-graph | Publish the graph image with digest and an upgrade/rollback check | none |
| E3 | aq-dashboard | Versioned dashboard release archive with provenance and capability metadata | none |
| E5 | aq-server | VPS and Cloudflare provisioning runbook, executed | E1b, E1c |
| E6a | aq-worker | Local WSL2 worker in a container with a rootless sandbox daemon and autostart | C2, B5b |
| E6b | aq-worker | VPS cloud worker on the docker-local backend | C2, B5b, E1c, E5 |
| F1a | aq-server | Supervisor host container and supervisor session lifecycle under central identity | B6a1 |
| F1b | aq-server | Supervisor conversation dispatch and reply, and scoped supervisor tool credentials | F1a |
| F2 | aq-server | Supervisor and owner enqueue-work commands at v1 parity | G1b, G2 |
| F3 | aq-worker | Agent messaging end to end in sandboxes | B7 |
| F4 | aq-server | Automatic notifications to Discord with real-destination qualification | D3 |
| F5 | aq-server | Agent questions to the owner, with answers delivered back | F3, H4 |
| F7 | aq-server | v2 human CLI (`aqv2`) over the same domain operations | G1b |
| G1a | aq-server | Action union, common error envelope and operation lookup | none |
| G1b | aq-server | Migrate existing writes behind the action union; pending operations in bootstrap; fixtures | G1a |
| G2 | aq-server | Typed task and knowledge commands | G1b, B4 |
| G3 | aq-dashboard | Work and Knowledge write flows | G2 |
| G4 | aq-server | Overview aggregation, attention, search and provider quota | G1b |
| G5 | aq-dashboard | Overview full composition | G4 |
| G6 | aq-server | Results comments, feedback tasks, waivers and a configured media store | G1b |
| G7 | aq-dashboard | Results feedback, media and feed modes | G6 |
| G8 | aq-dashboard | Supervisor docked panel, full page and mobile | F1b, F2 |
| G9 | aq-dashboard | Playbook start, cancel and activation | D9 |
| G10 | aq-dashboard | Agents operations views: health, accounting, waits, terminal snapshots | H2, H3, H4, H7 |
| G11 | aq-dashboard | Visual acceptance against every named mockup, WCAG 2.2 AA, tablet breakpoints | G3, G5, G7, G8, G9, G10 |
| G12 | aq-dashboard | Bundle compatibility: digest check and draft preservation across update | E3 |
| H1 | aq-worker | Adopt observation readers and the durable observation store in the runner | B6c |
| H2 | aq-server | Production usage accounting and the pricing reference | H1, B4 |
| H3 | aq-server | Stall ladder with nudge | H1, F3 |
| H4 | aq-server | Durable waits registry and agent wait routes | B2 |
| H5 | aq-worker | Hibernation checkpoint and continuation bootstrap | H4, B5b |
| H6 | aq-server | Hibernation wake scheduling | H4, H5 |
| H7 | aq-worker | Terminal renderer snapshots and full output path | B6c |
| I4a | aq-worker | Codex CLI qualification with the owner's ChatGPT login in docker-local sandboxes | C6a, C2, B5a, C6b |
| I4b | aq-worker | Codex CLI ChatGPT-login qualification on the VPS worker, with refresh write-back | I4a, C6b, E6b |
| I1 | aq-worker | Claude Code adapter | C6b |
| I2 | aq-worker | Gemini CLI adapter | C6b |
| I3 | aq-worker | OpenCode adapter | C6b |
| J3 | aq-server | Measure the deployed system at about 10 concurrent attempts | D10, E6a, E6b, I4a |
| J4 | aq-server | Security review of permits, credentials, sandboxes and Access | C6b, E5, F1a |
| J5 | aq-server | Off-host encrypted backup and restore drill | E5 |
| J7 | aq-git | Supported repository profile decision and widening | A3a |
| MS0 | aq-server | MS0 gate: first real agent run | E0, I0 |
| MS1 | aq-server | MS1 gate: real agent, sandboxed, local | MS0, A3a, A3b, B1, B2, B3, B4, B5a, B5b, B6a2, B6b, B6c, B7, C1, C2, C3a, C3b, C3c, C4, C5a2, C6a, D1a, D1b, D3, I4a, B6a1, C5a1, D0, C5b, C6b |
| MS2 | aq-server | MS2 gate: hosted | MS1, A1, A4, C5b, C6b, I4b, E1a, E1b, E1c, E2, E3, E5, E6a, E6b |
| MS3 | aq-server | MS3 gate: daily driver | MS2, C7, D2, D4, D5, D6, D7, D8, D9, F1a, F1b, F2, F3, F4, F5, F7, G1a, G1b, G2, G3, G4, G5, G6, G7, G8, I1 |
| MS4 | aq-server | MS4 gate: complete product | MS3, D10, D11, G9, G10, G11, G12, H1, H2, H3, H4, H5, H6, H7, I2, I3, J3, J4, J5, J7 |

---

## Revision log

- **2026-11-02, initial plan:** 79 tasks.
- **2026-11-02, after an independent Fable review and the owner's scope decisions:**
  - Added MS0 (E0, I0).
  - Split B5, B6, C3, C5, C6, D1, E1, E6, F1, G1 and I4 into session-sized tasks.
  - Named-test deliverables throughout.
  - Fixed edges: I4 needs B5; G8 needs F2; MS1 runs on E0; the cross-repo Review Focus test is split.
  - The MS2 cloud worker uses `docker-local` on the VPS, and the local worker runs containerized on WSL2. boat moves to MS3, along with Claude Code.
  - Subscription logins are allowed on every backend (personal deployment).
  - Enterprise-scale items moved to the deferred backlog.
  - Runbook fixes: project id, `aq start`, dedicated clones, per-slot target dirs, `direct` integration, importer dry-run first.
  - Docs-steward rule.
- **2026-11-02, parallelization review (Fable):**
  - Removed the 14 MS0 release-gate edges.
  - Split B6a (B6a1/B6a2) and C5a (C5a1/C5a2).
  - Removed 6 over-constrained edges and added 17 missing ones.
  - Set priorities.
  - Raised the project cap to 8, the standard-high Codex pool to 6 and the operator Codex pool to 2.
- **2026-11-02, self-contained descriptions:**
  - Every task description now carries Goal, What to build (requirement text inlined), Current state, Interfaces, Out of scope and section-level References.
  - The footer forbids reading whole design documents and out-of-scope work, and blocked owner decisions go through `aq message send`.
  - Added D0 (aq-git GitHub remote credentials).
  - Added edges: D7←D6, D6←B5b/C4, D9←B4/G1a, B7←C3a, E1c←F1a/B5b/C2, F2←G2, C3a←A3a, D1b/D2/MS1←D0.
  - boat runs as a second enrolled VPS node, `aq-worker-boat`.

- **2026-11-02, subscription logins (owner):**
  - No API keys are required; agents use the owner's CLI subscription logins.
  - I0, I4a and I4b now qualify Codex on the owner's ChatGPT login, with refreshed logins written back to `~/.codex/auth.json`, because v1's Codex agents share it.
  - C5b gains a login-import command.
  - MS1 now needs C5b and C6b.
  - Owner questions everywhere go through `aq message send`.

## Appendix: gap analysis summary (2026-11-02)

| Gap | Summary | Addressed by |
| --- | --- | --- |
| G1 | Recovery and capacity guarantees proven only on the synthetic runtime; the real path was a one-shot runner | B5, B6, C1 |
| G2 | No native cancellation; one attempt per task ever; narrow scheduling; fixed base commit | B3, B4 |
| G3 | Completion only through human review; outbox unread; 3 of 12 playbook commands; no triggers; catalog drift | D3–D9 |
| G4 | No candidate/publish path; no repository onboarding; narrow aq-git repository profile | D1, D2, J7 |
| G5 | Agents lacked scoped AQ access and messaging; supervisor not hosted; manual-only notifications; no agent questions | B7, F1–F5 |
| G6 | Observation, accounting, stalls, waits and hibernation not running | H1–H6 |
| G7 | Product not assembled: Compose lacked graph and aq-git, dashboard mounted from disk, manifest format 1, aq-git pin missing from history | A3a, A3b, A4, E1–E4 |
| G8 | Only Codex, only against a fake provider | I1–I4 |
| G9 | Performance budgets missed or untested | D10, J1–J3 |
| G10 | Dashboard reads mostly live; actions and aggregation missing | G1–G12 |
