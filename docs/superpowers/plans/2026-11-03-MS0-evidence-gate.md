# MS0 Evidence Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish an accurate MS0 decision record for I0, applying the owner's conditional approval without inventing successful runs.

**Architecture:** This operator task assembles existing evidence and read-only stack observations in `docs/implementation/MS0.md`. No application code changes or real-provider reruns are required. I0 explicitly reports 0/3 successful runs, so the owner's condition requires a failed gate.

**Tech Stack:** Markdown, Python standard-library evidence inspection, Docker inspection and local HTTP GETs, AQ CLI, Git.

**Spec:** `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml`, MS0; held task `stark-zenith-16.1`; owner comment `comment-626f0ab3daf0494aa56f0074ddb52482`.

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

---

### Task 1: Assemble and publish the gate evidence

**Files:**
- Create: `docs/implementation/MS0.md` — decision, three-run matrix, findings, demo, verification and limits.
- Create: `docs/implementation/evidence/MS0/observations.json` — sanitized read-only stack observations.
- Create: `docs/implementation/evidence/MS0/I0-first-real-run.md` — immutable copy of reviewed public I0 evidence.

**Interfaces:**
- Consumes: aq-worker I0 report at published commit `17bb649`, E0 handoff, existing settlement API contracts and owner conditional pre-approval.
- Produces: MS0 evidence document and failed gate disposition if three successful retained and settled runs are absent.

- [ ] Confirm the report, inspect its provenance and record missing acceptance before writing the decision document. Run `test -f docs/implementation/MS0.md`; absence is expected initially. This is a documentation task: no synthetic application tests or Rust changes.
- [ ] Cross-check live graph task and attempt/source state through read-only public endpoints, where available. Save only allowlisted nonsecret observations; label unavailable checks explicitly.
- [ ] Collect all eleven filed I0 problems plus the server follow-up, proposing severity and MS1 effect where canonical task metadata is unavailable. Do not mutate another task or rerun I0.
- [ ] Write MS0.md with the exact owner comment, explicit 0/3 outcome, IDs and missing fields, image/source provenance, unresolved decision questions and executable demo commands with honest prerequisites and expected outcomes.
- [ ] Verify Markdown whitespace, local links, run-ID consistency and shell syntax of the demo. No named test deliverables exist. Full Rust/dashboard suites do not apply to documentation-only changes.
- [ ] Settle the received subtasks, recording nonapplicable code/TDD/adversarial-review steps as skipped with reasons. Commit the document and observations, merge `origin/main`, publish using `aq git push`, and record the full OID.
- [ ] Notify the owner of the concrete failed gate evidence using `aq message send --to user:dashboard --project aq-platform`. Close fail with the exact unmet three-run/source/settlement acceptance, evidence paths and verification commands, then obey the next-claim result.
