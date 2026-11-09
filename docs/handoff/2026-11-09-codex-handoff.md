# Handoff: AQ v2 swarm work (for Codex)

**Written:** 2026-11-09, by the Claude Code supervisor that ran this work from 2026-11-02.
**For:** the next agent, Codex. It assumes you have read nothing else yet.

## 1. Where things stand

AQ v2 is a set of Rust services, plus a TypeScript dashboard, that run coding agents in sandboxes. Its repositories are in `/home/jkern/dev/aq-platform/` (GitHub org `ElectricJack`):

| Repo | What it is | Agent guide |
| --- | --- | --- |
| `aq-server` | central server, Postgres, contracts, specs and plans for the whole program | `AGENTS.md` (read it first) |
| `aq-worker` | node worker: slots, sandboxes, session host, journal and recovery | the repo docs |
| `aq-dashboard` | the web UI | `AGENTS.md` (visual acceptance rules) |
| `aq-graph`, `aq-git`, `aq-playbooks` | supporting services; no open swarm work | the repo READMEs |

The current program is **spec 0007, the swarm** (`aq-server/docs/specs/0007-swarm.md`): always-on workers on every machine the owner has, cloud VMs when those aren't enough, and one Swarm page that shows and controls all of it. It has five phases (spec §10):

| Phase | State |
| --- | --- |
| 1. Workers as services | **Done and accepted live** (2026-11-08, live run 4, release build) |
| 2. Swarm page (read-only) | **Done.** The visual record stays *open* until the phase 3 controls bar exists |
| 3. Controls and placement | **Not started.** On hold for the owner (§4, item 1) |
| 4. Cloud VMs (boat) | Not started; depends on 1 and 3 |
| 5. macOS | Not started; depends on 1 |

Live acceptance of phase 1 found that a cap on *total* history rows stopped the swarm after 33 attempts. That led to **spec 0008, sealed-history retention** (`aq-server/docs/specs/0008-sealed-history-retention.md`):

- **Phase A is merged.** Caps now count only live rows, a rate limit replaces them as the brake, and a held node or project shows a visible reason.
- **Phase B, deleting closed history and keeping a small summary, waits for an owner answer** (§4, item 2).

v1 (the older Python agent queue at `/home/jkern/dev/agent-queue2`, CLI `aq`) **was shut down at the owner's request** on 2026-11-08. It tracked this program's tasks; nothing tracks them now apart from the documents below. **Do not restart v1, and never edit that checkout.**

No agents or PR checks are running. Every merged main is green.

## 2. Rules from the owner

These come from the owner and do not change without them.

**Security**
- **No API keys.** Agents use subscription logins only.
- **Secrets stay out of** repos, logs, messages, argv and PR text.
- **The Claude token** is at `~/.config/aq-secrets/claude-oauth-token` (mode 0600). It is passed only as the env var `CLAUDE_CODE_OAUTH_TOKEN`. Never print it.
- **The agent credential never reaches an agent's sandbox,** on any machine (spec 0007 §4 and §8).

**Hands off**
- Leave the owner's Supabase container and `aq-worker@local.service` alone. That service is the owner's own worker, not part of the swarm tests.
- Never edit `/home/jkern/dev/agent-queue2`.
- Never change global config or pools without asking.

**Writing and planning**
- **No time estimates,** anywhere. Give scope, order and dependencies.
- **Mark who decided what.** Decisions in specs are tagged **(owner)** or **(my call)**, and the owner reviews the second kind. Keep doing this.

**Re-reviews**
- **Judge only the fix commits** against the reviewed head. Do not run a fresh full review.

**Usage**
- **The owner pauses v2 work when weekly Claude usage reaches 80%** (rule from 2026-11-03). The last reading was 84%, which is why phase 3 is on hold. The rule is about Claude usage. Ask the owner whether it applies to your work before you start heavy work.

## 3. How the work was run

### Build rules

The build rules are in `aq-server/AGENTS.md` (owner requirements of 2026-11-05). In short:

- **A private `CARGO_TARGET_DIR` for each worktree.** Never share one: a shared target links another tree's code into your tests.
- **Every `cargo test` or `cargo nextest` runs under `timeout`,** for example `timeout 900 cargo nextest run ...`.
- **Iterate on one test binary.** Don't alternate `cargo build` and `cargo test`.
- **aq-server has 8 domain integration-test binaries** (81 in the workspace). Add tests to the existing binaries, not new ones.
- **Line ratchets only go down.** The scripts print each budget:
  - `python3 scripts/build-budget.py` in aq-server: the lib is at 109,103, which equals its limit.
  - the same check in aq-worker: last recorded at 70,876.
  - New code goes into new crates; the work moved code out of the aq-server lib to pay for itself.

### Branches and worktrees

- **One branch and one worktree for each change,** under `/home/jkern/dev/aq-platform/wt/<name>`.
- **Branch names:** `swarm/p<phase>-<topic>` and `fix/<topic>`.
- **Many old worktrees remain** in `wt/`. They are done work; check `git worktree list` before reusing one.

### Review before merge

1. Write a spec, which the owner approves.
2. Write a plan in `docs/superpowers/plans/`.
3. Implement on a branch and open a PR.
4. An independent reviewer gives a verdict: `MERGE`, `MERGE AFTER FIXES` or a rejection.
5. Run fix rounds, with a scoped re-review of only the fix commits each round.
6. Merge.

Findings are graded Critical, Important, Minor or Low. Minor and Low findings are **parked** in the ledger (§6), not fixed in the same PR.

### Merging

- **Squash merges only,** one PR at a time.
- **Main CI must be green between merges.** Don't stack merges on a red or pending main (retrospective W82).
- **A server contract change merges with its dashboard re-pin.** Re-pin with `scripts/pin-contract.py <server worktree> dashboard-read <merge sha>`, run from the dashboard repo. The dashboard pins only `dashboard-read`; `human-graph` is not pinned.
- **The owner approved merging green, reviewed PRs.** No other approval is needed.

### Rulings

When a plan didn't settle something, the supervisor decided it and wrote one line in the ledger: `Ruling: <decision> — <why> — <cost if wrong>`. Do the same, and bring anything irreversible or outward-facing to the owner first.

### Notes to keep

Commit both files to aq-server main with `[skip ci]`. Commit first, then `git pull --rebase`; never use `git stash`.

- `docs/research/2026-11-02-v2-build-supervision-log.md`: a timestamped entry for every intervention, merge or notable change.
- `docs/research/2026-11-02-v1-hosting-retrospective.md`: lessons, numbered W1…W88. Add new ones as W89 and later.

## 4. Waiting on the owner

The owner has been asked all four of these and hasn't answered yet. Don't decide them yourself.

1. **Start phase 3 now, or after the usage reset?** This is the 80% rule above.
2. **Spec 0008 §13, results and reviews in phase B.** A run's result report and its review threads are tied to the run's records by foreign keys, so phase B as written deletes them 7 days after the run settles and its task closes. Only the ~1 KB summary card would stay. The alternative is to keep them longer, or forever, which means cutting the result tables loose from the run records.
3. **Terminal input (W13), option A.** This adds a fourth startup profile, `native-exec-interactive-pty-v1`, to aq-session-host so the owner can type into a docker-local Claude agent's terminal.
   - The full write-up is in `aq-server/.superpowers/sdd/2026-11-07-swarm-p1-workers-as-services/w13-report.md`; the supervisor recommends A.
   - Step 2 of the phase 1 live acceptance is pending on this.
4. **The A1 soak.** This is Task A1 of the 0008 phase A plan: more than 600 attempts on aq-ms1, to prove the live caps never stop work. Run it now, or after the usage reset?

A fifth item needs owner review once it comes up: **option B, true resume.** Today a crashed attempt is retried from its last saved work (ruling C). True resume means unfreezing `stopped.rs` in aq-admission's compatibility baseline, a compatibility break.

## 5. Next work, in order

1. **Phase 3 plan (spec 0007 §5.3, §6, §7.1, §7.4),** once the owner agrees. It covers:
   - Settings › Swarm: provider maximums, the VM maximum and size, and per-machine maximum agents, CPU and memory thresholds and display name;
   - sliders that replace per-project pool sizes, including migrating B4 per-project replacement budgets;
   - the §7.1 placement rules: local machines first, then the VPS worker (my call), then a new VM;
   - visible waiting reasons;
   - the burn gauge. Claude shows "unknown" until a usage source exists; Codex uses the existing quota collector.

   Write the plan in the style of `docs/superpowers/plans/2026-11-07-swarm-p1-workers-as-services.md`: exact files, interfaces, tests and line-budget rows. Then implement it task by task with review.
2. **Update the ms1 stack to current main.** It runs server 4c4b3a00 and worker b8959743, which lack aq-server#121 (lock contention) and aq-worker#54 (retry on 503). See §7.
3. **0008 phase B** (spec §8), after the owner answers §4 item 2.
4. **A batch of parked minors** (§6), grouped by repo.
5. Phases 4 and 5.

## 6. Parked follow-ups

The full list is the ledger at `aq-server/.superpowers/sdd/2026-11-07-swarm-p1-workers-as-services/progress.md`.

- **The ledger is git-ignored,** so it exists only on this machine. Don't run `git clean -fdx` in aq-server.
- **It lives beside the reports** (`*-report.md`) and `w13-report.md`.
- **The phase 2 ledger** is `.../2026-11-07-swarm-p2-swarm-page/progress.md`.

**Worth doing early**
- **M2:** "Retry attempt" on a policy-held task does nothing but still reports "confirmed".
- **M3:** `release-sandbox` needs `--force` for a lost attempt the server already released.
- **M4:** task-view wording. A retired task shows "Ready", and the held status contradicts itself.
- **Launch-closure path:** the `launch_closure_pending` and `prepare_disposition_unknown` worker holds need an operator today.
- **The control-poll 503 isn't retried** by the worker. Preparation GETs and inventory, register and heartbeat are.
- **#121 lows:**
  - The 503 `overloaded` mapping for 55P03, 40P01 and 40001 applies globally. Add `Busy` to the match at `crates/aq-server/src/reconciliation/mod.rs:280`.
  - An owner action that hits the 1 s `lock_timeout` returns Internal; it should be retryable.
  - Unnamed pools force `idle_in_transaction_session_timeout=0`.
- **D6:** the handoff commit message. Separately, decisions grow by up to 3 per offer.

**#120 (visible holds) minors**
- An invisible stale hold row can still post a comment.
- The sweep doesn't stamp `checked_at` for holds with no `graph_id`, which starves it.
- A Graph 404 keeps the hold forever.
- The work view writes from a GET.

**Tests and CI**
- The guardian tests in aq-worker need tolerance for slow runners.
- A recovery-level test should show that Held keeps the sandbox.
- `git.rs`: `kill_group(child.id())` on the `start()` bail paths.
- The CI tests job takes about 21 minutes against a 10-minute budget.

The ledger also lists #117, #119 and #51–#54 nits.

## 7. The live environment on this desktop

**aq-ms1** is an isolated v2 stack used for live acceptance, on ports 28080, 28081 and 28741. It runs on WSL2 Linux.

- **Control script:** `~/.local/share/aq-ms1/up.sh [command]`. It wraps `scripts/ms1-demo.sh`, whose header lists the commands.
- **Build sources:** the worktrees `wt/ms1h-server` and `wt/ms1h-worker`.
- **Helpers:** `psql.sh` (a database shell), `rebuild-worker.sh` and `remeasure.sh`.
- **Backups:** a `backup-pre-*` directory precedes each live run. Take one before any destructive step.
- **Worker builds:** `rebuild-worker.sh` builds dev only. A release build is `cargo build --release --locked -p aq-worker -p aq-session-host -p aq-agent-client --bins`.
  - Use release for live runs. The dev profile once overflowed the recovery stack (R4).

**`aq-node@ms1.service`** (a systemd user unit) is the phase 1 worker service. It is installed, enabled and running on release binaries, enrolled as `desktop`.
- **Its service renewal is valid only until 2026-11-10T00:58Z.**
- Operator guide: `aq-worker/docs/operations/install-service.md`.
- Evidence and findings: `aq-worker/docs/implementation/0007-P1.md` and `aq-server/docs/implementation/0007-P1-evidence/`.

**`aq-sandbox-podman.service`** is the rootless sandbox daemon.

**`aq-worker@local.service`** belongs to the owner. Leave it alone.

## 8. Known flakes

These are not caused by current work. Rerun once; if a flake repeats, file it.

- **aq-server:**
  - `it_phase` frozen tests. The presence pool's 250 ms acquire and 750 ms statement timeouts are too tight under load; this deserves its own issue.
  - `playbook_activation` (×2–4) and the `delegated_permit_runtime` restart, under full parallel load.
  - a recovery-matrix guardian broken pipe.
- **aq-worker:** the `guardian.rs` 5 s waits on slow CI runners.
- **aq-dashboard:**
  - `knowledge-library.spec.ts:77` and `knowledge-qa-fixes.spec.ts:57`;
  - six terminal Playwright specs under local load. They pass with `--workers=2`.

## 9. Open PRs: check before touching

v1 agents opened these before the swarm work. They are stale and don't belong to this program. **Don't merge them without checking** that the change is still wanted and still applies to main.

- **aq-server:**
  - #3: workspace formatting checks;
  - #9: Clippy and duplicate example targets;
  - #53: per-project replacement budgets. Phase 3's sliders may supersede it;
  - #60: G11 knowledge relationships;
  - #63: D6 check-attempt execution;
  - #64: the Host7 judgment provider.
- **aq-worker:** #32, an opt-in sccache capability.
- **aq-dashboard:**
  - #55: a phone header;
  - #93: the Habit Hooks pilot, opened 2026-11-09. It is new and not part of this handoff; ask the owner.

## 10. Where to read

| What | Path (aq-server unless noted) |
| --- | --- |
| Swarm spec | `docs/specs/0007-swarm.md` |
| Retention spec | `docs/specs/0008-sealed-history-retention.md` |
| Phase 1 plan | `docs/superpowers/plans/2026-11-07-swarm-p1-workers-as-services.md` |
| Phase 2 plan | `docs/superpowers/plans/2026-11-07-swarm-p2-swarm-page.md` |
| 0008 phase A plan (includes Task A1, the soak) | `docs/superpowers/plans/2026-11-08-sealed-history-p-a-live-caps.md` |
| Phase 1 server notes | `docs/implementation/0007-P1-server.md`, `docs/implementation/0008-PA-server.md` |
| Phase 1 worker notes | aq-worker `docs/implementation/0007-P1.md`, `0007-P1-worker.md` |
| Swarm page mockup | aq-dashboard `.superpowers/brainstorm/2382938-1794083621/content/swarm-tree-v3.html` |
| Whole v2 program plan | `docs/superpowers/plans/2026-11-02-v2-full-application-plan.md` (+ `-tasks.yaml`) |
| Supervision log / lessons | `docs/research/2026-11-02-v2-build-supervision-log.md`, `docs/research/2026-11-02-v1-hosting-retrospective.md` |
| Ledger (local only) | `.superpowers/sdd/2026-11-07-swarm-p1-workers-as-services/progress.md` |

### Key mechanisms

Here is what phase 1 and 0008 phase A added, so you don't have to rediscover it:

- **Live attempts.** Migration 0103 defines a live attempt with a partial index. Caps count only live rows (migration 0104), and the indexes are attested exactly in the settlement39 catalog (`ATTESTED_INDEXES`).
- **The rate fence.** Migration 0105 allows 30 offers per node per hour, 240 per project per hour and 960 per hour in total. The poll gate in crate `aq-history-caps` answers 200 with no offers when it holds, and the hold codes are `history_rate_limited` and `history_cap_reached`.
- **Visible holds.** Migration 0106 adds `task_holds`, the view `task_holds_current`, a trigger on `execution_attempts` that clears a hold on a new attempt or release, and a 10-minute sweep. Each hold gets one system comment, and the dashboard shows `task_held` / `tasks_held`.
- **Lost-attempt release (ruling C).** A lost attempt is released with a `boot_changed` or `cgroup_teardown` proof. A crash is charged against the retry budget, but a reboot isn't. The sandbox is kept unsalvaged.
- **Owner decisions in force.**
  - A dashboard Stop doesn't count against retries.
  - Availability is sticky.
  - Enrolled machines are auto-allowed.
  - Thresholds use the WSL VM's load, with no Windows probe.
  - The Swarm page opens with the whole tree fitted.
  - One VM per agent attempt.
  - Credential custody follows spec 0006 §5.3, option 1.
- **Lock discipline (#121).**
  - Human reads use `REPEATABLE READ READ ONLY` snapshots.
  - Owner actions on many nodes lock `ORDER BY id`, with a 1 s `lock_timeout`.
  - A no-op availability switch touches nothing.
