# MS1 — real agent, sandboxed, local

Task `stark-zenith-16.2`. Run on 2026-11-05, 05:00–05:35 UTC, on the owner's
desktop (WSL2). Owner decision of 2026-11-04: the agent is **Claude Code**
signed in with the owner's Claude subscription login (no API key), not Codex.

## Owner verification

MS1 is merged: aq-server `1ac43cb1` (#94), aq-worker `b82b08af` (#41). The
`aq-ms1` stack was rebuilt from `origin/main` (aq-server `1185ec70`, aq-worker
`b82b08af`) on 2026-11-06, 02:15–02:37 UTC, and the demo below ran on it once
with real Claude Code.

### What MS1 demonstrates

On the owner's desktop (WSL2), with the owner's Claude subscription login and
no API key:

1. aq-graph tasks become AQ attempts. The node runs each one through
   `aq-worker run-native`: Claude Code runs in a `docker-local` sandbox, edits
   a worktree, and runs `cargo test`.
2. The node retains the result commit in central aq-git. D3 accepts it,
   and D1 publishes it to the project remote with a fast-forward of `main`.
   Only then does D3 complete the task.
3. A dependent task becomes ready only after that, and starts from its
   predecessor's published commit.
4. An owner cancel stops a running attempt. The sandbox is stopped and kept,
   and nothing is reported or published.
5. A failed attempt is reworked. Its partial commit is retained, and B4 admits
   a replacement in a new sandbox. The replacement completes the task.
6. The Claude token reaches the agent only as `CLAUDE_CODE_OAUTH_TOKEN` in the
   sandbox environment, never in argv, logs or files that the stack keeps. A
   token scan of the evidence, the node runs and the server log finds nothing.

The remote is a local bare repository in the aq-git volume
(`/var/lib/aq/remotes/aq-scratch.git`, cloned from `ElectricJack/aq-scratch`).
Pushing to GitHub is not part of MS1 (see Known limits).

### Evidence of the hand-off run

Stack `aq-ms1`: <http://127.0.0.1:28080/>. Run-native node `ms1-node` =
`188de335-4fc9-4e85-b15b-138f01999775`; spare enrollment `ms1-node-spare` =
`085affc7-…`. Raw files (run logs as `run-*.txt`) are in
[MS1-evidence/handoff/](MS1-evidence/handoff/). Wall times are from
`run-native`.

| Step | Task | Attempt | Result report | Outcome |
| --- | --- | --- | --- | --- |
| A: add `hello()` | `e0a20000-…-000000000011` | `ffa8b8c3-d38e-5113-a54d-afd9b8b56bd0` (40 s) | `753cae4f-7557-40f1-a010-3d62a3d0be76` | published `2e32645b` → **`6c43decb`**; D3 confirmed, Graph operation `3868c15c-60b3-4628-a7be-9647233abcb0`; task `completed` |
| B: README line (blocked by A) | `e0a20000-…-000000000012` | `42f352c0-42d6-54a8-9082-43180971fcc1` (40 s) | `2c7241d7-bf68-4c5b-83ce-ceb1a3a5fe76` | started from **`6c43decb`**, published `6c43decb` → `e0ab6fe4`; D3 op `779dc8de-dab9-4e21-ba91-36a27df974fc` |
| Cancel | `e0a20000-…-0000000001c1` | `2f28ecd7-9d6c-548f-94e9-4560cb5bc38a` | none | owner cancel at 02:31:12Z, cancellation `8e47f505-1314-41a6-aedf-9d995b722848` (`requested`); sandbox stopped and kept (`Exited (137)`); cancelled settlement recorded; nothing published; task stays held |
| Fail | `e0a20000-…-0000000001d1` | `eabac549-fbf7-526d-a70b-ab6096251a77` (62 s) | `56b91d17-e4d9-4a25-bc19-5d746a350e14`, `failed` | Claude killed in its sandbox at 02:32:45Z after 3 of 6 files (operator-injected crash); D3 `rework_required`; replacement facts recorded (source retained, effects quiescent) |
| Retry | same task | `4b995ff1-c9f0-5bd3-832a-d7ba9779abb3`, new sandbox (117 s) | `e7c5062a-e6d9-47a6-836c-ebe987ebfcf8`, `success` | "replacement attempt admitted within the lineage budget … attempts_started=1"; published `e0ab6fe4` → `22a13087` (all 6 files); D3 op `ff5aaeb3-87f6-442b-bf4d-92abbb24cf62` |
| Measured run | `e0a20000-…-0000000001e1` | `9a5da87e-4e88-5473-868c-986193edacce` (40 s) | `f9ccb524-8339-40ea-bbc4-0d64d3e23667` | published `22a13087` → `38bee5f`; D3 op `540b438c-e74e-4a5b-9d31-b901020382a2` |

The remote's `main` is now `38bee5f` → `22a13087` → `e0ab6fe4` → `6c43decb` →
`2e32645b`: five fast-forwards, no merge commits.

### Rerun each step

The stack's own wrapper, `~/.local/share/aq-ms1/up.sh COMMAND`, runs
`scripts/ms1-demo.sh` from `wt/ms1h-server`. It sets the aq-worker source
(`wt/ms1h-worker`), its target directory and the credential
(`owner-claude-subscription@2`, revision 2). D1 is the default in the script.
New tasks use `scripts/ms1-task.py` with `AQ_MS1_HOME=~/.local/share/aq-ms1`
and `AQ_DEV_GRAPH_URL=http://127.0.0.1:28741`.

```sh
U=~/.local/share/aq-ms1/up.sh
$U status                      # 4 healthy containers, nodes, sandbox count, URLs
$U graph                       # tasks A and B: state, version, ready
$U publication                 # delivery evidence, source heads and the remote's main

# Steps A and B: two dependent tasks, B builds on A's commit
python3 scripts/ms1-task.py ID-B "title" "body"                 # create the dependent task first
python3 scripts/ms1-task.py ID-A "title" "body" --blocks ID-B   # A blocks B
$U run                         # runs A: Claude in a sandbox, retention, D1 publication, D3
$U run                         # runs B, from A's published commit
$U publication                 # two `applied` publications; B's expected_old is A's commit

# Cancel: a slow task, then cancel its attempt while the sandbox runs
python3 scripts/ms1-task.py ID-C "slow task" "Create 10 files, sleep 15 between them, then cargo test"
$U run &                       # wait until `docker -H unix:///run/user/$(id -u)/aq-sandbox/podman.sock ps` shows the sandbox
A=$(~/.local/share/aq-ms1/psql.sh -c "select attempt_id from execution_attempts where task_id='ID-C'")
curl -sS -X POST -H 'Content-Type: application/json' -H 'X-AQ-Request: 1' \
  -d "{\"operation_id\":\"$(cat /proc/sys/kernel/random/uuid)\",\"reason\":\"owner cancel\"}" \
  http://127.0.0.1:28080/api/execution/attempts/$A/cancel

# Fail, then retry: a slow task, kill Claude in its sandbox mid-run, then run again
python3 scripts/ms1-task.py ID-D "slow task" "Create 6 files, sleep 10 between them, then cargo test"
$U run &                       # after a few files:
docker -H unix:///run/user/$(id -u)/aq-sandbox/podman.sock exec aq-NODE-ATTEMPT pkill -KILL -x claude
$U run                         # the replacement, in a new sandbox; completes the task

# Release the sandbox kept by the cancel (operator path, records the reason)
~/.local/share/aq-ms1/runtime/aq-worker --config ~/.local/share/aq-ms1/node/config/worker.toml \
  release-native-sandbox --native-config ~/.local/share/aq-ms1/node/config/native.json \
  --attempt-id 2f28ecd7-9d6c-548f-94e9-4560cb5bc38a --reason "owner reviewed the cancel drill"

# Memory and CPU around any run
scripts/ms1-stats.sh /tmp/ms1-stats & $U run; kill %1; $U stats-summary /tmp/ms1-stats
```

**Fresh stack from scratch** (what the hand-off did). The dev vault and the
seed's repository designation are immutable for a Compose project, so a
stack keeps its first mode.

1. `$U up`. If the copied `intelligence.toml` has `[result_media]`, first
   create `/var/lib/aq/result-media` (0700, uid 10001) in the
   `aq-ms1_dev_source` volume.
2. `$U seed`: the local bare remote, the project, tasks A and B, and the
   E0 placeholder task retired.
3. `$U node-enroll ms1-node`, then `$U node` (writes `worker/node.json`; the
   persistent controller it starts is fenced) and `$U node-stop`.
4. `$U profiled`, `$U import-login`, then `echo $((REV+1)) >
   ~/.local/share/aq-ms1/revision` and `$U profiled` again with credential
   revision 2, then `$U import-login --replace`. The first import stages the
   base file, which changes the registered credential state. Without the
   second `profiled`, the server answers 409 "credential rotation/state change
   requires increasing credential revision".
5. Every later pinned change (`rebuild-worker.sh --driver`, a new server
   image) needs a revision bump and `$U profiled` before `$U rebuild-server`.

Incarnation 3 of `aq-ms1` (the B4/B5b runs of 2026-11-05) was archived to
`~/.local/share/aq-ms1-inc3`, with `pg_dump`s of `agent_q` and `aq_graph` and
tarballs of its volumes in `volumes-backup/`. Stack `aq-ms1d` (D1) is stopped
and its data is kept (`docker compose -p aq-ms1d start` restarts it).

### What to look at in the dashboard

- **Overview** (`/`): counts of running, ready, need-you and recovering work.
  "Needs your attention" lists the failed process results (the E0 placeholder
  and the injected crash) and the offline workers.
- **Work** (`/work`, choose "Local scratch development"): the five MS1
  tasks. A, B, the retry task and the measured task are `Completed`. The
  cancel task stays open, because a cancelled attempt has no retained source
  to replace.
- **Agents** (`/agents`): one row per attempt with harness `claude`, model
  `claude-sonnet-5-5`, central state `Finished` and start and stop times. The
  live state reads "State unknown" and "Controller unknown" (see Known limits).
- **Results** (`/results`): each MS1 result appears with its summary, its changed files and its checks and evidence. This was fixed by aq-dashboard #71 (d5c3bc4d), which re-pins the results-feedback contract; a contract mismatch now degrades only the Results view. The retry run's change set (task `…01d1`) lists `target/` build output; see Known limits.

### Step 6: Windows suspend drill

The node-level helper:

1. Start the persistent controller: `$U node`. `$U status` must show
   `ms1-desktop online`. Note the time (`date -u`).
2. Run `$U drill`. The first run rebuilds aq-worker's `claude_live` test
   binary (a few minutes). When it prints `drill: sandbox aq-… running;
   suspend Windows now`, choose Start → Power → Sleep at once.
3. Wait at least 60 s, then wake the machine and log in. Leave the WSL
   terminal alone until the drill finishes. It prints `drill: wall-clock gap of
   N ms detected` after the wake.
4. Check:
   - **Timings:** `suspend_windows` (wall and monotonic gap) and `elapsed_ms`
     in `~/.local/share/aq-ms1/evidence/drill-*/suspend-drill/suspend-drill.json`.
   - **Continuation:** `runner_exit` `Exited(0)` means that the sandbox survived
     and finished; `write_calls` 100 means no work was lost.
   - **Reconnects:** `drill-*/controller.log` and `drill-*/server.log` around
     the gap.
   - **After the wake:** `$U status` shows a fresh `last_seen_at` and no
     leftover sandbox.
5. `$U node-stop`.

The same drill with a real graph attempt:

1. Create a slow task, as in the cancel step, but with about 20 files and
   `sleep 15` between them. Run `$U run`.
2. When the sandbox shows in `docker -H unix:///run/user/$(id -u)/aq-sandbox/podman.sock ps`,
   suspend Windows. Wake it after at least 60 s.
3. Observe one of three outcomes:
   - **Continued:** `run-native` ends with `AttemptStopped` and the task
     completes. `$U publication` shows the commit.
   - **Expired:** the grant expired during sleep. The attempt is stopped, its
     report is `failed` or missing, and the next `$U run` takes a B4
     replacement in a new sandbox.
   - **Held:** the run-native output says `retained state preserved` and the
     task stays held. Run `aq-worker reconcile-native` on the node state;
     `$U node-enroll` is the fallback for a wedged node.
4. Record which outcome happened, the wall gap (`date -u` before and after)
   and the server log (`docker logs --since 30m aq-ms1-server-1`).

### Measured memory and CPU (hand-off run)

`scripts/ms1-stats.sh` during the measured run (task `…01e1`), in
[stats-summary.txt](MS1-evidence/handoff/stats-summary.txt):

| Component | Peak memory | Max CPU |
| --- | --- | --- |
| postgres | 314.7 MiB (includes page cache after six runs) | 8.9% |
| aq-server | 16.8 MiB | 8.1% |
| aq-git | 8.0 MiB | 3.7% |
| aq-graph | 6.3 MiB | 3.4% |
| Stack total | 343.4 MiB | — |
| Sandbox (Claude plus `cargo test` on a one-file crate) | 159.7 MiB (cgroup peak) | 23% of one core |

Earlier figures (see [Memory and CPU](#memory-and-cpu)): a sandbox peaks at
about 530 MiB with a trivial Rust build, and the node controller has 26.5 MiB
RSS. A realistic Rust build has not been measured (E5).

### Known limits and owners

| Limit | Owner |
| --- | --- |
| Copy-back takes every file the agent leaves, so a repo without a `.gitignore` for build output publishes it. The retry commit `22a13087` includes `target/`. Copy-back and publication need a build-output/ignore policy, and must not rely on the agent running `cargo clean` | aq-worker sandbox driver (C6a), D1a policy |
| GitHub push: D1 publishes to a local bare remote; pushing to GitHub needs named credential profiles in the pinned aq-git and central credentials | A3b (`eager-grove-45.3`), D2 (`crisp-meadow-36.3`) |
| The persistent `aq-worker run` controller is still fenced; attempts run through one-shot `run-native` | no open plan task (see [Missing integration](#missing-integration), item 1) |
| No live session or terminal state reaches Agents ("State unknown", "Controller unknown") | not assigned |
| A cancelled task stays held: B4 needs retained source, which a cancelled attempt never produced | B4 (`noble-summit-37.4`) |
| Selection's 1-hour lineage budget refusal fails the poll instead of skipping the lineage | B4 |
| Reviewer follow-up: `release_fence` is not scoped to its owner | B4 |
| Reviewer follow-up: the sandbox-uncertain path skips recording retention, so such an attempt is never replaceable | B4, B5b (`noble-summit-37.6`) |
| Reviewer follow-up: the S4 block (`replacement_after_unpublished_delivery_needs_new_source`) gives the owner no clear action | D1b (`crisp-meadow-36.2`) |
| Reviewer follow-up: no test for a failed source-head compare-and-swap | D1b |
| Reviewer follow-up: check whether delivered token material survives on a preserved sandbox disk | C5b (`calm-delta-18.8`) |
| The two old wedged nodes (`c7caa999…`, `f051a29d…`, archived with incarnation 3) cannot be closed: old terminal qualifications and recipe identities are no longer installed | B5b/config |
| Following external pushes to `main`; replay of a held result after a server restart; reconcile of an indeterminate publication; PR mode | D2, D1b |
| Templates are unadmitted; the sandbox daemon is single-UID | C3c, I4a owner fix |

Owner verification: **signed off by the owner on 2026-11-06** ("I'm good with MS1 at this point"), after the dashboard QA pass and fixes (aq-server #97; aq-dashboard #74–#80).

## Update 2026-11-05 (late): final-review fixes on `ms1/retry`

The independent review of the converged head asked for these on
`ms1/retry` (the publication items are on `ms1/d1`):

- **B1, build budget.** `long_phase_manifest` is now a module of the
  aq-admission `preparation_v2` binary. `ms1_live` and `sandbox_driver_live`
  are modules of aq-worker's `claude_live` binary, still `#[ignore]`d, and
  `ms1-demo.sh` runs them there. The test binary counts are back at budget
  (81 and 105). The lib line ratchets are raised, each in its own commit with
  its reason: aq-server to the measured value, aq-worker to 71,448.
- **S2, duplicate effects.** The host's stop proof covers only its own
  process tree, and the host kills the driver within its 1 s grace, while a
  container stop takes about 10 s. The Claude executor now stops every
  sandbox of the attempt, and confirms the stop, before the host stop and
  before Stopped is reported. If the stop is not confirmed, Stopped is
  withheld: the attempt is stop-uncertain, no replacement facts are
  recorded, and the task stays held. `reconcile-native` replays Stopped only
  after it has confirmed the sandbox stop.
- **S3, credentials.** The driver reads and unlinks the token before anything
  that can fail. After the host stops, the executor and `reconcile-native`
  remove any leftover `driver-*/token`. If the token's exact bytes appear in
  a copied-back file, path or link, in the transcript, or in a pane line, the
  driver refuses both the copy-back and success, and reports Failed with a
  fixed reason. The pane line is withheld, and `claude.jsonl` is kept, in the
  private driver directory, only when it is token-free.
- **S5.** The closure tests now cover the "host stopped, no process ran"
  variant, a host stop whose root ran, and the held cases (execution history,
  effects). A missing, unreadable or malformed `exit.json` maps to Unknown.
- **Nits.** Copy-back refuses an entry below a symlinked parent and a link
  whose target is absolute or uses `..`. The paneless closure rule's comment
  and unit test are corrected.

The aq-ms1 stack still runs the driver from before these changes. Deploying
them needs `rebuild-worker.sh --driver`, a revision bump, `profiled`, and
then `rebuild-server`.

## Update 2026-11-05 (21:20–21:46 UTC): B4 retry and B5b closure settlement live

Server image from `ms1/retry` (merged with `main` and `ms1/d1`); D1 stays
off on `aq-ms1` (`AQ_MS1_D1` unset). Migration **0095** is applied (state
format 95).

### B4: a failed attempt is retried in a new sandbox and D3 completes the task

Task `e0a20000-0000-4000-8000-000000000103` (slow 10-file task, priority 30):

| | Attempt 1 (failed) | Attempt 2 (replacement) |
| --- | --- | --- |
| Attempt | `452394d1-32f7-5308-8901-e0a6c35d33a8` | `3440bb36-d8b1-522d-af70-4c188a902cd7` |
| Grant | `90937684-32e0-4816-a7ac-1b121579f1db` | `1372aea5-67a4-4116-b8f7-6d52e9de3bfc` |
| Sandbox | `aq-10112d04…-452394d132f753088901e0a6c35d33a8` | `aq-10112d04…-3440bb36d8b1522daf704c188a902cd7` (new) |
| What happened | Claude SIGKILLed in its sandbox at 21:20:11Z after 2 of 10 files (operator-injected harness crash); driver `exit.json` `success:false` | Claude ran to the end: result `success`, `is_error:false`, 22 turns, session `e5e6a1e8-92b6-42d5-9e37-5ca78a733e1e`; `exit.json` `success:true` |
| Retained source (central aq-git) | `9d9adfcb6107b88a007085f361047f3b9ae8c16d` (partial work) | `b162f4bee92eab2c76d773fc04b6d3716473a8b3` |
| Report | `36b7dd7c-bd03-4619-8c5c-a517631ab29a`, outcome `failed` | `ab86777c-f177-40f2-adee-1e342d44a348`, outcome `success` |
| D3 | `rework_required`/`not_accepted`; then (21:20:24Z) replacement facts recorded: retention `retained`, effects `quiescent` | `confirmed`, Graph operation `a080377a-a709-4194-8318-a61e5925759e`; task `completed` (v2) |

At 21:20:41Z the poll admitted the replacement: "replacement attempt admitted
within the lineage budget … attempts_started=1 lineage_elapsed_ms=66848
sandbox_ms_charged=910683 max_attempts=3 max_sandbox_ms=14400000", and B4
released the predecessor's claim (`profiled_replacement_releases`,
`attempts_started=1`). The predecessor's sandbox was destroyed after its
copy-back; the replacement ran in its own sandbox, which was destroyed too.

**Budget enforcement, observed:** drill 2 (`…0102`) failed at 15:03Z; its
replacement at 21:18Z (after the pause) was refused by selection because the
1-hour lineage time budget of the selection policy was spent (B4's own server
cap is 24 h). Follow-up (B4): that refusal currently fails the poll instead
of skipping the lineage, like the project-revision case fixed earlier.

### B5b: a granted attempt that never started is closed, settled, released and replaced; its node re-registers

Task `e0a20000-0000-4000-8000-000000000104`, node `10112d04…`:

1. Induced fault: `aq-session-host` made non-executable for one run.
   Attempt `4b19839c-bf0a-5f9f-9ed2-350ea585c7c6` (boot `e82391b9…`) was
   granted, then "guardian spawn uncertain: Permission denied" (21:30:59Z):
   granted/stop unknown, task held.
2. The node is wedged: the next run's registration was refused (21:31:13Z)
   with "prior execution claims remain held".
3. Worker closure: `aq-worker reconcile-native` on the preserved state
   recorded a durable suppression and posted it (21:32:38Z, "granted launch
   closed before Started"): closure `7dbac2e5-fb4e-43f9-aecf-43fd0d6fcaf7`,
   evidence `suppressed`.
4. Server settlement (sweep, 21:32:46Z): status `finished`, stop
   `not_started` (`aq.launch-closure/7dbac2e5…`), retention `retained`
   (`aq.source-export/98c352d0-99d1-5ed0-ab3d-1d1f6afb963f`, its own input
   export), effects `quiescent`.
5. Release (sweep after the new image, 21:44:47Z): "closed launch settled and
   its task claim released for B4 replacement": `profiled_replacement_releases`
   row, `attempts_started=1`.
6. The same node, with its existing enrollment (`node/config/worker.toml`),
   re-registered with a new boot (`6c3d3b53…`) at 21:45:28Z and took the
   replacement: attempt `a9b73699-8ff3-5279-a1c5-e07b5fb1336f`, report
   `92009cd6-f3fc-4247-824b-da0aa5fd574a` (`success`), retained source
   `278a93645b15643b2fe6feb8c3be602516fef366`, D3 `confirmed` (Graph
   operation `dc17116a-9128-4efb-9daf-6c76c4f350bc`); task `completed` (v2).

**Closure rule for "host started, harness never launched" (the `aq-ms1d`
case, attempt `59c31201…`).** Settling it the same way is safe only when the
host's own stopped proof shows that no process ever ran under it: no root
status, no reaped process, and the kernel wait sees no children. That holds
when the observed gate failed closed before exec, pane or not, so the
server now accepts it (commit `7aed79ba`). The worker sends that proof:
`reconcile-native` recovers a started guardian from its sealed intent, stops
it and retains the kernel stopped proof before the closure. A host whose pane
ran anything stays held for an operator. If the host cannot be recovered (no
host state left), there is no proof, and the attempt cannot be closed
automatically.

**The two old wedged nodes (`c7caa999…`, `f051a29d…`) were not closed.**
Their attempts (`30383974…`, `69de1b52…`) were granted under runtime
qualification version 1. The server only installs the current terminal
qualification, so it refuses their closure with "phase terminal owner
mismatch". Their journals also no longer open with the current worker,
because the renamed RUNNER changed the recipe identity (`result recipe
identity`). A worker built from `0ee3b25` with `88c98f7` cherry-picked got past the
second problem but not the first. Owner: B5b/config. The fix is to keep
historical terminal qualifications and recipe identities installed for
recovery, or to add an owner abandonment for closed-but-unstopped attempts.
Those two nodes stay retired; nothing new is placed on them.

### Tests (after merging `main` and `ms1/d1`)

- **aq-server, nextest:** 66 passed across `it_profiled`, `it_phase` and
  `it_native`. The subset covered `auto_settlement56`, `replacement_source`,
  `replacement_release`, `phase_terminal_closure`, `phase_execution`,
  `native_cancellation`, `native_offers`, `native_registration` and
  `profiled_source`. The `failed_or_unknown_report_never_completes…` guard is
  green.
- **aq-server, lib:** 30 passed for `replacement`, `closure::settlement`,
  `login_import`, `expiry` and `settlement`. `clippy --all-targets -D
  warnings` and `fmt --check` are clean.
- **aq-worker:** 440 lib tests passed, including the new `unstarted…` and
  `refresh_profile…` closure tests. `--test templates` passes. Clippy and
  fmt are clean.
- **Flaky under load only:** `native_cancel_owner_validation_attempt_replay_and_concurrency`,
  `final_insert_wait_expiring_facts_rolls_back…` (aq-server) and the journal
  test `unknown_attempts_and_changed_acceptance_retries_do_not_consume_revision`
  (aq-worker) each failed once while a Docker image build ran beside them.
  All three passed when rerun alone.

## Update 2026-11-05 (21:00–21:30 UTC): D1 publication live, B builds on A

Branch `ms1/d1`. Plan:
[2026-11-05-ms1-d1-publication.md](../superpowers/plans/2026-11-05-ms1-d1-publication.md).
Stack `aq-ms1d` (ports 38080/38081/38741, state `~/.local/share/aq-ms1d`), a
separate instance; `aq-ms1` and `aq-v2-local` were not touched.

**Closes finding 3.** D3's default completion policy now runs with
`delivery = required`. When D3 has accepted a result and is waiting only for
delivery evidence, the new D1 delivery owner publishes the retained commit
through the project's D1b policy (`fast_forward`, `refs/heads/main`),
records the publication evidence, and only then does D3 complete the task. The
project source is a named ref on `main`, which resolves to the head AQ
published, so the dependent task starts from its predecessor's commit.

The remote is a **local bare repository** in the aq-git volume
(`/var/lib/aq/remotes/aq-scratch.git`, cloned from `ElectricJack/aq-scratch`,
`allow_file_transport`). aq-git pushes to it with the same lease-guarded push it
would use for GitHub. Pushing to GitHub needs D0 credential profiles in the pinned
aq-git (A3b) and central credentials (D2).

| | Task A2 | Task B2 (blocked by A2) |
| --- | --- | --- |
| Graph task | `e0a20000-0000-4000-8000-0000000000d1` → `completed` v3 | `e0a20000-0000-4000-8000-0000000000d2` → `completed` v3 |
| Attempt / node | `4f71c639-3a11-596f-809e-af4e20073355` / `51834301-b318-4e42-8ba5-26dddacb2cc5` | `a2e66fd3-ad98-5c8c-9df2-5ec92f2239fe` / same node |
| Source admission | named ref `refs/heads/main` → `2e32645b` (fresh central sync; base retention handle `e59c3eb1…`) | named ref `refs/heads/main` → **`7b3bad0c`** (publication head evidence; candidate retention handle `1d61d805…`) |
| Worker-side base (transfer manifest) | `2e32645b` | **`7b3bad0c`** (A's commit) |
| Claude session | `da621257-733c-465c-be91-8b2fd32f8e15`, 4 turns, 11.0 s, $0.032 list | `149d115d-373f-4734-b2ec-8acd51e21d8a`, 4 turns, 8.9 s, $0.029 list |
| Work | `Cargo.toml`, `Cargo.lock`, `src/lib.rs` (`hello()` + test), `.gitignore` | one README line; Claude reported "step 1's `hello()`, its unit test and `.gitignore` were already present and left unchanged", `cargo test` passed |
| Retained checkpoint (import op) | `7b3bad0c279659c5443914ecc0aae0a12344d534` (`dfbe0de1-a7d8-41b7-ab5b-c934636d3f16`), parent `2e32645b` | `d4138d2b015fb3a73979f5debd16cc6a0042f293` (`636df314-f301-4d8c-8885-bc8f2298da79`), **parent `7b3bad0c`** |
| Result report | `947eb9ad-feba-437a-a37a-42204b858996` | `e49bd48f-76ec-4b24-99a3-11a053992c4a` |
| Candidate (`candidate.create` op = aq-git candidate id) | `1d61d805-79e2-594d-ac9f-c9d0991ca252` → `7b3bad0c` (source descends from the base, so the candidate is the checkpoint itself) | `7a87cb9b-edfa-5817-b23e-ed3e77d251aa` → `d4138d2b` |
| Publication (`publish` op) | `da61f20b-8466-588e-9158-d40e4f7a3452`: **`applied`**, `main` `2e32645b` → `7b3bad0c`, acknowledged 21:24:24.751Z | `ad069841-f152-5d8a-8c65-e38a4d088c9d`: **`applied`**, `main` `7b3bad0c` → `d4138d2b` |
| Delivery evidence / source head | `published`, recorded 21:24:24.819Z; source head sequence 1 = `7b3bad0c` | `published`; source head sequence 2 = `d4138d2b` (previous `7b3bad0c`) |
| D3 settlement | `confirmed` 21:24:24.893Z (after the evidence), graph operation `02388878-06fe-4acf-91be-dade0812ed18` | `confirmed`, graph operation `77995948-b98d-4f97-8073-5a17d7e81e45` |

The remote's `main` after both runs is `d4138d2b` → `7b3bad0c` → `2e32645b`: two
fast-forwards, no merge commit, no re-push. B2 became ready only after A2's
publication was recorded and D3 had confirmed it. All three runs ended with the
sandbox stopped and destroyed, and the leased login wiped. A token grep (pattern
fed from the file on stdin) over the evidence, the node runs, these docs and the
server log finds nothing.

**Code** (aq-server `ms1/d1`; aq-worker and aq-git unchanged; no migration):

- `settlement/publication.rs`: the delivery owner. It runs detached from
  the result request (`results::record` spawns it when D3 holds on
  `delivery_evidence_missing`), so a dropped client cannot cancel a push
  mid-dispatch.
  - Sources: only the report's server-confirmed retained receipt.
  - Candidate base = `expected_old` = the recorded source head, or before the
    first publication the commit the attempt was admitted on.
  - Operation IDs derive from the report, and a retry replays the D1a frozen
    inputs.
  - Outcomes: `applied`/`already_present` → `published`;
    `target_moved`/`rejected`/conflict → `unpublished` (D3 rework); a lost or
    invalid reply after dispatch → `indeterminate` (D3 holds; reconcile only).
  - Transport errors, a blocked destination, `recovery_blocked` and
    pull-request mode write nothing and stay held.
- The source head is stored in the `published` delivery evidence with a
  sequence. It advances by compare-and-swap from the `expected_old` it
  published over, under a per-target advisory lock. A migration-free choice:
  0096 is reserved, and migrations must be numbered without gaps.
- `source.rs` / `source/ownership.rs` / `source_exports.rs`: a named-ref
  source resolves to the recorded head and hydrates from its candidate
  retention handle, accepted only for exactly that commit.
- `producer/build.rs`: the offer builder admitted only immutable sources (live
  refusal at `build.rs:236`). It now admits a named ref at the exact commit it
  resolved to, which is frozen into the export key and re-checked against the
  export base.
- Scripts: `ms1-demo.sh` takes `AQ_MS1_PROJECT` and the ports as parameters;
  `AQ_MS1_D1=1` selects the local remote, the named-ref source,
  `delivery=required` and the publication policy; `remote`, `sync` and
  `publication` commands were added. `ms1-profiled.py` gains
  `AQ_MS1_CREDENTIAL_REVISION`.

**Tests.** `it_profiled profiled_results::auto_settlement56` 10/10, including:

- `retained_result_publishes_then_completes_and_advances_source`: fake
  aq-git peer, evidence published, head advanced, D3 confirmed, replay does no
  aq-git I/O;
- `moved_target_is_unpublished_rework_and_never_advances_source`.

Lib tests `source::`, `settlement::publication`, `producer::` 20/20 (including
`named_ref_follows_publication_head_only_for_its_exact_commit_and_handle`).
`fmt --check` and `clippy -D warnings --all-targets` are clean.

**Final-review fixes (publication items, after the live run above):**

- **S1, no publication of a cancelled task.** The owner used to check D3's
  acceptance only once, before it built the candidate. It now re-checks
  acceptance immediately before `publish`, under the report lock, the D3
  settlement lock and the node/attempt/hot locks that cancellation and
  abandonment take: current report head, Success, not revoked, not cancelled,
  still holds its task, stopped, D3 still holding on missing delivery evidence.
  - In that same transaction it writes a durable fence: an `indeterminate`
    evidence row in phase `publishing`. While that row exists, an owner cancel
    returns the new disposition `already_publishing` and records no
    cancellation, and abandonment is refused.
  - A cancel that lands before the fence makes the owner skip the publish, and
    D3 reworks the result as `not_accepted`.
  - The final evidence replaces the fence. A refusal proven to come before
    dispatch removes the fence. A crash or unknown reply leaves the result held.
- **S4.** B4 now blocks the replacement of a task whose lineage has a
  `delivery_unpublished` rework, with reason
  `replacement_after_unpublished_delivery_needs_new_source`; a successor would
  reuse the same immutable export and hit the same conflict. A blocked ready
  task now logs its reason (`ready task not offered`, debug).
- **Nits.** `already_present` with an observed target other than the candidate,
  and a published head whose compare-and-swap fails, are now recorded
  `indeterminate` (held), never `published`.
- **S5 tests** (`it_profiled` `profiled_results::auto_settlement56`):
  - `non_success_or_cancelled_report_never_reaches_aq_git`;
  - `cancel_between_candidate_and_publish_is_never_published`;
  - `cancel_during_publishing_is_recorded_as_already_publishing`;
  - `ambiguous_or_foreign_presence_stays_held_without_a_source_head`;
  - the B4 block is asserted in
    `moved_target_is_unpublished_rework_and_never_advances_source`.

**Findings (with owners):**

1. **Transient host bootstrap failure wedged a node.** The first A attempt
   (task `…0011`, attempt `59c31201-20a5-5c72-adc5-487e95c6a17e`, node
   `3d19e214…`) was granted. The session host then stopped with "observed gate
   failed closed; post-grant exec evidence may be unknown", and the worker
   reported "native host bootstrap refused: reader closed or malformed packet".
   The host writes no diagnostics. An `aq-ms1` launch started in the same
   second; the same binaries then worked on a fresh node.
   - `reconcile-native` refuses ("runner genuine Running absent") because the
     guardian had started, so the node cannot re-register ("prior execution
     claims remain held") and task `…0011` (and B `…0012` behind it) stays held.
   - Owner: B5b/AQ-08 closure (a started guardian that never reached
     Running) and B6a (host diagnostics).
2. **Following external pushes** to `main` is not implemented: the head is
   AQ's last publication. An external push makes the next publication
   `target_moved` (rework), never an overwrite. Owner: D2 (central sync owner
   that retains the observed head).
3. **No replay** of a result held across a server restart (D3/D1b), and no
   automatic reconcile of an indeterminate publication (D1b). PR mode stays
   refused (D1b/D2).
4. A dedicated source-head table belongs at the next free migration; the
   evidence-row lookup is unindexed (D1b).

**Rerun** (`~/.local/share/aq-ms1d/up.sh` sets the project, ports, `AQ_MS1_D1=1`
and credential revision 2):

```sh
~/.local/share/aq-ms1d/up.sh status
python3 scripts/ms1-task.py ID TITLE BODY [--blocks OTHER]   # AQ_MS1_HOME / AQ_DEV_GRAPH_URL for aq-ms1d
~/.local/share/aq-ms1d/up.sh run           # owner observation, central sync, one attempt
~/.local/share/aq-ms1d/up.sh publication   # delivery evidence, source head, remote main
```

## Update 2026-11-05 (15:30 UTC): B4 retry and B5b closure settlement (paused)

Work paused by the owner (Rust build optimisation) before the replacement
attempt ran to completion. D1 moved to another agent (`ms1/d1`). Migration
**0095** (`replacement_source_after_never_granted`) is on `ms1/retry`.

**Shown live on `aq-ms1`:**

- **A failed attempt gets replacement facts and a replacement offer (B4).**
  Task `…0102`, attempt `3b46ffaf-0b5a-5732-accf-8d2a3451bf78`: Claude was
  SIGKILLed in its sandbox at 15:03:16Z (operator-injected harness crash, 2 of
  10 files written). The driver copied back, the sandbox was destroyed, the
  report (`f8f16b49-9d07-4aba-981a-f56408f3f234`) is `failed`, D3 recorded
  `rework_required`/`not_accepted`, and the D3 policy actor then recorded
  `retention=retained` and `effects=quiescent`. The next poll admitted a
  replacement: server log "replacement attempt admitted within the lineage
  budget … attempts_started=1 … sandbox_ms_charged=908199 max_attempts=3
  max_sandbox_ms=14400000", and the predecessor's claim was released
  (`profiled_replacement_releases`). The same happened for drill 1 (task
  `…0101`, attempt `667855dc-74a4-5683-a5e1-65b2c18f46a1`, report
  `3b9dfd5d-41e6-4cd2-9325-fb3395df891c`).
- **Backfill.** The sweep recorded the facts for the two attempts reworked
  before this owner existed (`a038d83a…`, `8181d51e…`).
- **Preserved sandboxes released by the operator.** `aq-worker
  release-native-sandbox` destroyed `aq-10112d04…-785d8c76…` (cancel drill) and
  `aq-10112d04…-4e2f1083…` (node-kill drill) with the operator's reason as
  destroy evidence; no sandboxes or volumes remain. Their tasks (`…00c2`,
  `…00e1`) were abandoned through the owner disposition route
  (`8c980e52…`, `4fe42546…`) and retired in Graph. Tidy-up of other demo
  tasks: `…0021` (`5ada26d6…`), `…00c1` (`d31a4140…`), `…00d2`
  (`0bd7f0d0…`) abandoned and retired; `…0101` retired.

**Real defects the live replacement exposed, fixed on `ms1/retry`:**

1. A never-granted successor poisoned the shared export: the 0053 guard
   refused every later successor (migration 0095).
2. A replacement across a project binding revision change failed offer
   assembly and so failed the node's whole poll, starving all other tasks
   (`assess_task_for_revision` now reports it blocked).
3. The phase-v2 Prepare refused any lineage elapsed time other than 0, and
4. required exactly one started attempt, so no replacement could ever be
   prepared (both generalised; first attempts keep the old rules).
5. Zero-capacity cancellation polls were refused once the inventory aged
   out, so the executor saw "unknown" instead of "not cancelled".

**Finished live at 21:20–21:46 UTC: see the next update above.**

**Dashboard.** `[result_media]` is configured on `aq-ms1`
(`ms1-demo.sh media`; the bootstrap now reports `results.media.read`/`upload`
enabled). `/results` still shows "Dashboard update required" with the
current bundle: the server's results-feedback items carry
`feedback_read_version`, `follow_up_submission` and `directly_following`
(aq-server `4eef26ea`), which the dashboard's pinned closed
`results-feedback/v1` schema does not allow. Owner: aq-dashboard (re-pin the
contract). `aq-v2-local` only loads because it has no results yet.

## Update 2026-11-05 (14:30 UTC): re-review fixes on `ms1/retry`

The independent re-review of `ms1/demo` found that B1 was only half closed.
Fixed on `ms1/retry` (aq-worker), each as its own commit:

- **B1 remainder (required).** Claude can exit 0 without doing the work: it
  can end its turn with a clarifying question, a refusal or "couldn't find X".
  Copy-back then succeeded, the executor reported `Success`, and D3 completed
  the task (no required checks, delivery not required). The driver now writes
  `exit.json` success only when all of these hold: the runner exited 0; the
  last stream-json `result` event has `subtype == "success"` and
  `is_error == false`; the agent's final text has its own `Changes:` line; and
  the copied-back change set is not empty. Anything else is `Failed`, and D3
  sends it to rework. A unit test covers each refusal case.
- **Template scan (CI).** The sandbox credential scanner refused the
  aq-worker binary because `RUNNER` contained a credential-labelled shell
  assignment (`credentials=false`). The flag is renamed `login_file`, and the
  `parse_token` fixtures no longer look like Anthropic tokens. `RUNNER`
  changed, so the pinned `CLAUDE_RECIPE_DIGEST` is now `b30030fb…8765`.
- **Recommended.** After a node kill, `reconcile-native` stopped the session
  host but not the docker-local sandbox, so Claude kept running there with the
  token in its environment. It now also stops (never destroys) the attempt's
  sandbox through `stop_attempt_sandbox`.
- **Nits.** The cancellation poll sends the node's real inventory sequence,
  not 1 (with 1, the server refused every poll that had no pending
  cancellation). Committed `__pycache__/*.pyc` files are removed and ignored.
  On the server, only a Claude subscription login (`subscription-login`,
  provider `anthropic`) has its CR/LF trimmed; API keys stay byte-exact.

**Still agent self-report (follow-up).** With no required checks, D3's
automatic completion still rests on what the agent did and said: the driver
guard refuses an obvious non-result, but a wrong change that Claude reports
as done still completes the task. The real backstop is required checks, which
the server runs against the retained source (D6). Until D6 lands, projects
that must not complete on self-report should keep a human review requirement.

## Update 2026-11-05 (08:00–13:30 UTC): review fixes, cancel, restart drills

Evidence: [MS1-evidence/wiring/drills.jsonl](MS1-evidence/wiring/drills.jsonl)
and the `run-*.log` files beside it.

### Review fixes (aq-server review of `ms1/demo`)

- **B1 (blocking): a failed run could complete its task.** Fixed both sides.
  aq-worker `e657e5f`: `ResultStopCause::Failure` reports `Failed`; the Claude
  driver writes `exit.json` (runner exit 0 observed by the backend, workspace
  copied back) *before* submitting its claim, and the executor reports
  `Success` only from that record; an unproven end stays `Unknown`. aq-server
  `98761e38`: D3 accepts `Success` only again (the earlier `Unknown`
  acceptance, `9ea067ad`, is reverted). New DB test
  `failed_or_unknown_report_never_completes_a_task_without_required_checks`
  (production-shaped policy-actor offer, no required checks): Failed and
  Unknown go to `rework_required`/`not_accepted` with no Graph write and the
  hold kept; Success confirms. Live: task `…00f1`, attempt
  `6e442551-d6a3-5812-808e-b800f47649d0` reported `success`, D3 confirmed
  (operation `f456b096-3be6-44ed-8122-9fc162a7713e`).
- **Should-fix 1** (`3ba235ed`): `require_container` is admitted only for
  `aq.claude-result-local-adapter` and only when the node's capability
  snapshot advertises `aq.harness.container-confinement`; contradictory
  profiles are refused; the poll workflow checks that the node's authenticated
  registration qualifies the exact confinement mode of the built manifest.
- **Should-fix 2** (`98761e38`, `cdb2a271`; aq-worker `e657e5f`, `2d43cf2`): the
  long-phase Claude rule also requires the container-confinement capability
  (and not native); aq-admission bumped to 0.1.12 in both repos (the long
  contract publication id stays `aq-admission/0.1.11`, so its pinned digest is
  unchanged). Test `long_phase_claude_manifest_requires_container_confinement`.
- **Should-fix 3** (`98761e38`): `CLAUDE_RECIPE_DIGEST`
  (`b3d428f7…026d0`) is pinned in `producer/result_startup.rs`; another recipe
  body under the Claude profile is refused (test).
- **Should-fix 4**: naming the policy actor for default-policy offers removes
  the human review fallback for those tasks. Recorded here and in the
  `review_requirement` comment; a follow-up, no code change.
- **Nits** (`3ba235ed`, `fd671591`): refusal-location logging is `debug`; the
  module-wide clippy allow is gone; version-0 Claude material is normalised
  like an import and `ms1-demo.sh` pipes the raw token file;
  `protocol/central-profile/v1/compilation.example.json` regenerated.

Tests: aq-server `profiled_results` 91 passed (1 ignored), `native_offers`,
`native_result_startup`, `native_accepted_preparation`,
`native_proposal_scope`, `native_cancellation` all passed against a disposable
Postgres (`aq-ms1-test-db`); aq-selection, aq-profiles, aq-admission and
aq-phase-terminal-protocol passed; clippy `-D warnings` and fmt clean.
aq-worker: 221 lib tests (`test-support`) passed; clippy and fmt clean.

### Step 3, cancel (B3): done

The Claude executor now polls the node's own poll at zero capacity every 5 s
(aq-worker `6ab152e`); the server answers pending cancellations first.

- Task `…00c2`, attempt `785d8c76-5261-59d3-9b11-811ab17ff2ac`. Owner cancel at
  08:05:37Z (`POST /api/execution/attempts/{id}/cancel`), cancellation
  `6c42e9be-f765-4203-83e7-fb1da4118c0e`, disposition `requested`.
- The executor observed it at 08:05:37.94Z and stopped the session host; the
  driver stopped its sandbox, which is **preserved** (`Exited (137)`, volume
  kept), not destroyed. `run-native` ended at 08:05:41Z.
- Server: attempt `finished`, stop `stopped`, cancelled settlement recorded
  (stop receipt digest `e2af7bb9…`). No result report, so it was **not**
  reworked and not completed.
- Limit: the task stays held: B4 replacement needs retained source, which a
  cancelled attempt never produced (see Step 4).

### Step 4, fail → retry (B4): not shown (server gap)

`replacement::assess` admits a replacement only when the predecessor's hot
state has `retention = retained` and `effects = quiescent`. No production
owner writes those for native attempts: every native attempt here, including
the completed ones, keeps `retention: pending` / `effects: unknown`. So a
failed, reworked or cancelled native attempt is never replaced; its task stays
held. The fix is a server-side writer (on a confirmed source retention receipt
and an empty `pending_effect_ids`), owned by B4/D3; not attempted here.

### Step 5, restart drills: done

- **Worker node killed mid-attempt.** Task `…00e1`, attempt
  `4e2f1083-510a-551c-b480-0a984a341031`. `run-native` got SIGKILL at
  08:08:55Z. The session host and driver kept running; Claude finished in the
  sandbox (exit 0 at 08:10:34Z); the guardian then stopped the harness at its
  local permit deadline (no renewer). Restart: `reconcile-native` could not
  recover a stop the journal never saw, so aq-worker `447984d` makes it recover
  the host owner from its sealed intent, retain the kernel stopped proof and
  replay Stopped. Result at 08:14:07Z:
  `StopReported`; server `finished`/`stopped`. **No relaunch, no second
  sandbox, no duplicate effect.** The node then re-registered with a new boot
  and ran task `…00e2` (attempt `214e5691-be59-56de-9402-3e4e2cc7da17`, D3
  confirmed, operation `f16f1cc3-1423-4823-b47b-40f4d0c4934c`). Limits: the
  killed attempt's edits were not retained (its sandbox is preserved,
  `Exited (0)`), and its task stays held (Step 4 gap). This is
  stop-and-preserve, not re-adoption: the native path has no re-adoption.
- **aq-server restarted mid-attempt.** Task `…00e3`, attempt
  `8730d7f5-e90e-5495-a06c-9f5b96a2a87b`. `docker restart aq-ms1-server-1` at
  08:15:50Z, healthy at 08:15:52Z. The attempt kept running (the node holds a
  15-minute grant and needs no renewal), its Started/Stopped/result reports
  landed after the restart, and D3 confirmed (operation
  `d3996f1f-4b4d-4743-9b0f-6c03acaebc6a`, checkpoint `816d5ab7…`). Exactly one
  result report exists. While the server was restarting, the cancellation
  polls answered 409 (logged; they never count as "not cancelled").

### Stuck attempts (close consumer): not done (server gap)

The server's `/v1/worker/profiled-v2/launch-closure` records a no-Started
closure as `LaunchClosedAwaitingSettlement`: it revokes authority and sets the
attempt `cancelling`, but deliberately retains the task, source and effect
holds, and no owner settles a closure afterwards. `compute::busy_locked`
still counts such an attempt as busy, so a worker-side closure client alone
would not let the node re-register. Granted-but-never-started attempts
therefore still wedge their node (nodes `c7caa999…` and `f051a29d…` here).
What does now unwedge a node: a Started attempt whose node died is recovered
by `reconcile-native` (above). Owner: B5b/AQ-08 closure settlement (a release
owner for launch closures plus the worker closure client).

### D1 publication (B builds on A): not done

Out of time in this window; finding 3 above still applies.

### Preserved sandboxes left on the daemon (operator release)

`aq-10112d04…-785d8c76…` (cancel drill, `Exited (137)`) and
`aq-10112d04…-4e2f1083…` (node-kill drill, `Exited (0)`), kept as
stop-and-preserve evidence.

## Update 2026-11-05 (06:00–08:00 UTC): execution wiring, steps 1–2 through the stack

Plan: [2026-11-05-ms1-execution-wiring.md](../superpowers/plans/2026-11-05-ms1-execution-wiring.md).
Evidence: [MS1-evidence/wiring/](MS1-evidence/wiring/). This supersedes the
"not met end to end" verdict below for steps 1 and 2; the older text is kept as
history.

**What now works live on `aq-ms1`:** a ready graph task is selected by aq-server
(profiled path), offered to the desktop node, accepted and granted; the node
leases the Claude login from the server vault; the session host starts the
node-side sandbox driver, which runs **Claude Code 2.1.318 in a docker-local
sandbox** (C2 fixed launch policy, rootless Podman) with
`CLAUDE_CODE_OAUTH_TOKEN` in the `claude` process environment only; the edited
worktree is copied back into the node's aq-git worktree, retained in **central
aq-git** as a checkpoint commit, the result is reported, **D3 completes the task
automatically**, and the dependent task becomes ready and runs the same way.

| | Task A3 (step 1) | Task B3 (step 2) |
| --- | --- | --- |
| Graph task | `e0a20000-0000-4000-8000-000000000031` → `completed` v3 | `e0a20000-0000-4000-8000-000000000032` (blocked by A3) → `completed` v3 |
| Attempt | `55b3ef7e-16d3-5b44-9799-e126ed4d46be` | `e80ed3b4-9813-5dfd-bbb0-3ced1ac55cff` |
| Grant | `45475232-0985-42ad-8d81-0aa7191eaf76` | `9d8fce12-ef44-4f04-9047-9140e242c0f9` |
| Node / boot | `10112d04-0e99-4597-9b20-c305d2bc219e` / `07989d35-a6aa-4152-89b5-26393d723f50` | same node / `3a8ef267-db97-44e6-9d9b-aaec5935509f` |
| Sandbox | `aq-10112d040e9945979b20c305d2bc219e-55b3ef7e16d35b449799e126ed4d46be` | `aq-10112d040e9945979b20c305d2bc219e-e80ed3b498135dfdbbb03ced1ac55cff` |
| Template | `sha256:194e11f54fbe104734166ec219f5ac13b2e54055fae01f6c3ce264a915f172a4` (I4a claude-rust-1, unadmitted) | same |
| Claude session | `4b1afcf7-910f-49d2-87a4-2e4d9c99946b`, 3 turns, 8.7 s, $0.027 list | `f5dce251-ed15-4036-9820-89ce2304b529`, 3 turns, 14.9 s, $0.025 list |
| Work | `Cargo.toml`, `src/lib.rs` (`hello()` + unit test), `.gitignore`; `cargo test --offline` passed in the sandbox | one README line documenting `hello()` |
| Retained commit (central aq-git) | `6d9c318922250127c723364e098ffe0684ca565c` (parent `2e32645b`) | `e41b1a002b9bf1bdacdc5f10f719be8edf2d07f0` (parent `2e32645b`) |
| Result report | `b2bb230b-08c6-43b1-9ef0-900a2fe9e06a` | `b8eb4dd2-4d14-4564-85d0-a783de237ea0` |
| D3 settlement | `confirmed`, graph operation `43dc1373-9bd3-493b-a5aa-19cf0644c232`, hold released | `confirmed`, graph operation `f3d79d8d-a61e-413a-933e-fb2a4654f6e1`, hold released |

Wall time per `run-native` invocation: 39 s (A3) and 45 s (B3). After each run
the sandbox was stopped and destroyed (no sandboxes left on the daemon) and the
leased login material on the node was wiped. A token grep (pattern from a pipe)
over the evidence, the server logs, the node state and both branches' recent
history finds it only in the node's own 0600 material store while an attempt
runs; the demo now wipes it after each run.

**Tasks A/B renumbered.** The original A (`…0011`) and two debug tasks are
wedged by attempts that were granted while the executor was still failing (see
finding 3); A3/B3 have the same titles and bodies (A3 adds an explicit
`.gitignore` hint). The E0 seed task `…0004` was parked behind B with a
`blocks` edge so it is not selected first.

### How it is wired (the minimal path)

The proven MS0 profiled pipeline (`aq-worker run-native`) is unchanged on the
server side of the wire; only the harness host changed. The node still holds
the grant and talks to the server; the sandbox holds no node credential and no
permit, only the attempt's leased token.

- **aq-worker** (`ms1/demo`): new executor kind
  `claude-2.1.318-docker-local-result-local-v1` (`native/runner/claude_docker.rs`,
  `native/launch/claude_docker.rs`): the private session host runs the pinned
  `aq-worker sandbox-harness` driver (`sandbox_driver.rs`) as its pane program.
  The driver creates the sandbox through `DockerLocalBackend`, uploads the
  worktree (no `.git`) and the C6a recipe/settings, delivers the token as
  private material, starts the fixed runner (reads the token, deletes the file,
  exports it to `claude` only, `IS_SANDBOX=1`, `bypassPermissions` in container
  confinement), streams Claude's events to the pane, copies `/aq/work` back
  (hard links supported), stops and destroys, then submits the AQ result claim
  from Claude's final message (also on failure).
- **aq-server** (`ms1/demo`): configuration via `scripts/ms1-profiled.py`
  (`[intelligence]` + `[profiled_execution]`, catalog/binding, owner
  availability observation, central base retention) and these code changes,
  each needed by a real refusal on the path:
  1. **D3 never applied to a real offer**: the producer hard-coded
     `ReviewRequirement::HumanOwner`. Offers now name the policy actor when the
     validation policy is the default completion policy.
  2. **D3 refused every native report**: the native result pipeline reported
     `ProcessOutcome::Unknown` by design. *Superseded by review B1 (see the
     newer update): D3 accepts Success only, and the worker now reports Success
     only for a proven harness exit 0 with its claim.*
  3. The long (schema 3) phase contract and the result-startup opt-in admitted
     only interactive Codex; they now also admit the Claude docker-local adapter
     (`aq-admission` 0.1.11 mirrored in aq-worker's vendored copy).
  4. Container confinement was derivable only from B1 placement facts, which a
     `run-native` node never registers; execution profiles may now set
     `confinement.require_container` (implies `permit_container`).
  5. C5b finding 2: `import-login` trims trailing CR/LF from Claude tokens and
     refuses other shapes.
  6. Diagnostics: every opaque profiled refusal logs its source location
     (target `aq_refusal`); a failed profiled policy install names its cause.
- Dev tooling: `AQ_DEV_REPLACE_OPERATOR_SECTION=1` replaces only the appended
  operator section of a retained dev vault (and the settlement config), so the
  stack keeps its seed, nodes and imported login. No migrations.

### Steps 3–5 through the stack

- **Step 3, cancel (B3): partial.** Owner cancel of attempt
  `a038d83a-19a1-5e3c-8582-b43688e6f2e2` (task `…00c1`) at 07:41:57Z was
  recorded (cancellation `953c42f7-0b8d-4f56-8b47-976d7965655a`), but the native
  executor never observed it: its drive loop neither renews the grant nor polls
  cancellations, so Claude ran to completion (156 s, 41 tool calls). D3 then
  refused the result (`rework_required`, `not_accepted`) and the task was not
  completed. Owner: B3/B5b (cancellation observation in the native executor).
- **Step 4, fail → retry (B4): not shown.** After that rework the next poll
  made no replacement offer (`NoOffer`); B4 replacement needs a proven-failed
  attempt, which a D3 rework is not. Owner: B4/D3.
- **Step 5, worker/server restart mid-attempt: not run** through the stack in
  this window.

### Findings (with owners)

1. **No worker closure for a granted attempt that never starts.** A launch
   that fails before Started leaves the attempt `granted`/stop `unknown`
   holding its task, and the node cannot re-register a new boot ("prior
   execution claims remain held"); `reconcile-native` needs a genuine Running.
   The server's `/v1/worker/profiled-v2/launch-closure` has no worker consumer.
   This wedged nodes `c7caa999…` (ms1-desktop) and `f051a29d…`; runs moved to
   freshly enrolled nodes. Owner: B5b / AQ-08 closure.
2. **A stopped attempt with no recorded claim** also holds its task forever
   (task `…0021`). The driver now always submits a claim. Owner: B4/D3.
3. **Each B depends on retained, unpublished A work.** The project source
   policy pins the immutable base `2e32645b`, and D1 publication is not in
   this path (`delivery = not_required`), so B3 started from the base without
   A3's `hello()` (Claude noted it). Making B build on A needs D1 publication
   to the remote (or a central branch) and a ref-following source policy.
   Owner: D1a/D1b.
4. **Non-interactive host permits are capped at 120 s** (session host
   `MAX_PERMIT_MS`); the Claude executor renews the local guardian permit
   (bounded by the grant) and the native journal accepts a local permit
   deadline at or below the grant deadline. Owner: B6a1.
5. Still open from the first run: delegated permits and the in-sandbox
   `sandbox-run` runner (B2/B5a/B6a–c) are not on this path; `aq-worker run`
   stays fenced. C5b finding 1 (first import needs the base file) is
   unchanged: provisionability requires a configured base fingerprint, which
   is a C5b design point, not a one-line fix. Template admission (C3c) and the
   single-UID daemon (I4a) are unchanged.

### Rerun

From `wt/ms1-server` with the stack up and the login imported:

```sh
~/.local/share/aq-ms1/rebuild-worker.sh --driver      # build aq-worker; install the pinned driver
echo $((REV+1)) > ~/.local/share/aq-ms1/revision     # only after a pinned program changed
AQ_DEV_REPLACE_OPERATOR_SECTION=1 scripts/ms1-demo.sh profiled   # config, server recreate, catalog/binding
AQ_MS1_CREDENTIAL=owner-claude-subscription@2 scripts/ms1-demo.sh import-login   # once per credential version
python3 scripts/ms1-task.py ID TITLE BODY [--blocks OTHER]   # new tasks
scripts/ms1-demo.sh run      # one attempt: owner availability observation, aq-git, run-native
scripts/ms1-demo.sh run      # the next ready task
scripts/ms1-demo.sh node-enroll NAME   # replace a wedged node (see finding 1)
```

## Verdict

**MS1 is not met end to end.** A real Claude Code agent does run in a
docker-local sandbox on this desktop, driven by the B5 node controller with the
owner's subscription token, and the sandbox side of cancel and node restart
behaves safely. But no graph task can travel from aq-server to that sandbox:
the production worker paths that would carry it are deliberately fenced, and
no open task owns the composition. So steps 1–5 could not run *through the
stack*, and nothing in this document claims they did.

| What | Result |
| --- | --- |
| Isolated v2 stack (`aq-ms1`) from `origin/main`, seeded with the scratch repo and two dependent tasks | **Done** |
| Node controller (`aq-worker run`, docker-local, Claude template) enrolled and online against it | **Done**, but admission is fenced: it never receives work |
| Claude credential imported into the server vault | **Done** with C5b's `aq-server import-login` (merged during this run): stored version 1 of `owner-claude-subscription@1` |
| ... and leased to an attempt | **Not done**: nothing on `main` can lease it to a docker-local sandbox |
| Real Claude edit → check → publish in a docker-local sandbox | **Done at node level** (controller + real backend, no server; publish goes to a bare repo inside the sandbox, not aq-git) |
| Steps 1–5 through aq-server (graph task → attempt → grant/permit → sandbox → aq-git → D3 completion → next task) | **Not done**: blocked by the gaps in [Missing integration](#missing-integration) |
| Step 6 (Windows suspend) | Written [procedure](#step-6-windows-suspend-drill-owner-procedure), with a helper; not run by the agent |

## Environment, commits and digests

| Item | Value |
| --- | --- |
| Host | Linux 6.18.33.2-microsoft-standard-WSL2, 24 CPUs, 31 GiB RAM; Docker Desktop (host stack) and the dedicated rootless sandbox Podman 5.8.8 at `/run/user/1000/aq-sandbox/podman.sock` (single-UID mode, see I4a) |
| aq-server | Branch `ms1/demo`. The steps 1–5 runs used `df51c21abab304bb86bf76ff0a7b390c2e8bf509` (`origin/main` at the start), image `sha256:850efceb85fddaf4b28712cfa4b921cb633772cf408acecbad518f916a4ea160`. The running stack and the credential import use `2b1133512bddfe4fbdb08700a8f7bb9702688604` (`origin/main` with C5b, PR #86), image `agent-q-server:ms1` = `sha256:833eaa49f566864ec616cd60fa9f7c9cb3814b36e5f32a50d4dcbfb8b89cd7b0`. Both are local builds with no registry digest. `aq-server 0.1.0`, bundled `aq-git 0.2.0` |
| aq-graph | `aq-graph:225329cd6acf5a775fd2a0b016dd1c45fb16c79b` (pinned in `compose.dev.yaml`) |
| Postgres | `postgres:17-bookworm@sha256:839fbb5a4942a7758dd93e18b42ea5649807491a0545b5eeeb8f0b8bb11857a5` |
| aq-worker | `19ce4cde5ac3a04e7d48cfd3e7830d10b9ffbbdc` (`origin/main`) plus the `ms1/demo` test commit; `aq-worker 0.5.1`, debug build run on the host |
| Template | `127.0.0.1:5138/electricjack/aq-template-unadmitted@sha256:194e11f54fbe104734166ec219f5ac13b2e54055fae01f6c3ce264a915f172a4`, local image ID `sha256:45937ad6e4ee283342774d758823e49de1bd61080c874a245b73c3e59565721b`. This is I4a's `claude-rust-1`: the C3c `rust-1` template (`sha256:4c9c2000…62a1c5`) plus Claude Code. **Unadmitted**: the C3c scanner refuses on this host (GnuTLS) |
| Claude Code | `2.1.318 (Claude Code)`, asserted inside the sandbox; model `claude-sonnet-5-5` (subscription default) |
| Dashboard | `aq-dashboard` `3685c0d` bundle, mounted through `compose.dashboard.yaml` |

The `aq-v2-local` stack (18080/18081/18741) and the `aq-worker-local` container
were not touched.

## The aq-ms1 stack

| Endpoint | URL |
| --- | --- |
| Dashboard and admin API | <http://127.0.0.1:28080/> |
| Worker API | <http://127.0.0.1:28081> |
| aq-graph | <http://127.0.0.1:28741> |

**Stack incarnations.** The dev vault is immutable for a Compose project, so
adding the `[intelligence]` section (the Claude credential) needed a fresh
project. That is why the IDs differ between runs:

- **Incarnation 1** (05:11–05:33 UTC, server `df51c21a`) produced the step
  evidence below. Node `4eb8af55-54a4-4220-a487-9dda14fa3efa`; graph change set
  `20cb4e7f-371c-4369-8625-9e64f867a8f9`.
- **Incarnation 2** (05:33–05:40 UTC) was discarded: it lacked the base file C5b
  needs, described under [Credential handling](#credential-handling).
- **Incarnation 3** (05:40 UTC onward, server `2b113351`) is the one left
  running. It has the imported credential. Node `ms1-desktop` =
  `c7caa999-6d7b-4e5b-926c-cc80993aef76` (online, boot `3c3238c8…`, admission
  fenced). The E0 seed worker is `52029868-70b2-4fb5-b467-935172fa4ba9`. The same
  task IDs A and B were created again in change set
  `818190d4-7a1d-42fd-969a-903a7af08109`: A ready, B blocked, both at version 2
  ([incarnation3.log](MS1-evidence/incarnation3.log),
  [status-final.txt](MS1-evidence/status-final.txt)).

Compose project `aq-ms1`. State, secrets and the generated dev vault are under
`~/.local/share/aq-ms1/{state,secrets}`. The node controller's config, enrollment
and journal are under `~/.local/share/aq-ms1/worker`. Raw evidence of every run
is under `~/.local/share/aq-ms1/evidence`. Copies are in
[`MS1-evidence/`](MS1-evidence/).

## Rerunning the demo

[`scripts/ms1-demo.sh`](../../scripts/ms1-demo.sh) runs everything above;
[`scripts/ms1-stats.sh`](../../scripts/ms1-stats.sh) samples memory and CPU.
From the aq-server checkout (worktree `wt/ms1-server`):

```sh
scripts/ms1-demo.sh up              # build and start aq-ms1 (about 3 min cold)
scripts/ms1-demo.sh seed            # scratch repo + the two dependent tasks
scripts/ms1-demo.sh node            # build aq-worker, enroll once, start the controller
scripts/ms1-demo.sh import-login    # store the Claude token (C5b); idempotent, --replace rotates
scripts/ms1-demo.sh live            # real Claude: steps 1, 3 and 5 at node level, with sampling
scripts/ms1-demo.sh server-restart  # recreate aq-server under the running controller
scripts/ms1-demo.sh gaps            # print the production fences and the stuck task
scripts/ms1-demo.sh status          # containers, worker status, sandbox count, URLs
scripts/ms1-demo.sh drill           # step 6 helper (see the procedure)
scripts/ms1-demo.sh node-stop       # stop the node controller
scripts/ms1-demo.sh down            # stop controller and stack; data is kept
scripts/ms1-demo.sh all             # up, seed, node, import-login, live, server-restart, gaps
```

Inputs, all overridable by environment: `AQ_MS1_WORKER_SRC` (aq-worker checkout,
default `wt/ms1-worker`), `AQ_MS1_CLAUDE_TOKEN_FILE` (default
`~/.config/aq-secrets/claude-oauth-token`), `AQ_MS1_TEMPLATE_IMAGE` /
`AQ_MS1_TEMPLATE_DIGEST`, `AQ_DEV_INTELLIGENCE_FILE` (default
`~/.local/share/aq-ms1/intelligence.toml`, a copy is in
[MS1-evidence/](MS1-evidence/intelligence.toml)) and `CARGO_TARGET_DIR`. The
intelligence file must exist before the first `up`: the dev vault cannot change
afterwards without `docker compose -p aq-ms1 down -v` and fresh state. The template image must already
be in the sandbox daemon (I4a builds and pulls it). `seed` and `node` are
idempotent: the graph mutation replays by idempotency key, and enrollment
happens only when `worker.toml` is missing.

The node controller runs from a private copy at `~/.local/share/aq-ms1/bin/aq-worker`
(log `~/.local/share/aq-ms1/worker/controller.log`, pid in `controller.pid`).
The Cargo target dir `target-ms1` was deleted after this run, so the next
`node`, `live` or `drill` first rebuilds aq-worker (a few minutes).
`~/.local/share/aq-ms1/up.sh [COMMAND]` is a shortcut for the demo script.

Each `live` run costs three short Claude sessions (about $0.03 each at list
price; subscription usage). `live` fails loudly if any live assertion fails.

## Credential handling

- The token file `~/.config/aq-secrets/claude-oauth-token` (0600) is read only by
  the live tests (`read_token` refuses group or world access). It goes into a
  zeroizing buffer and is delivered as the controller's private material, which
  is the `/aq/private/material` archive delivered into the sandbox. Inside the
  sandbox the fixed runner reads it into `CLAUDE_CODE_OAUTH_TOKEN`, **deletes the
  file before any work starts**, and exports the variable only to the `claude`
  process tree.
- It never appears in argv, container create environment, labels, logs, the
  evidence or the commits. Checks: each test asserts no read-back output holds it;
  step 1 scans `$HOME`, `/aq` and `/tmp` inside the sandbox (0 files); and a
  token grep over `MS1-evidence/` (pattern piped from the file, never argv)
  finds 0 files.
- Claude wrote no `~/.claude/.credentials.json` in the sandbox, and the host's own
  Claude login was unchanged (size and mtime, asserted by `claude_live`).
- **Imported into the aq-ms1 vault with C5b** ([import-login.txt](MS1-evidence/import-login.txt)).
  C5b (`calm-delta-18.8`, PR #86) merged into `main` during this run, and
  `ms1/demo` was merged up to it. The setup:
  - **Configuration.** [intelligence.toml](MS1-evidence/intelligence.toml) is
    non-secret. It enters the dev vault through the new opt-in
    `AQ_DEV_INTELLIGENCE_FILE` in `scripts/dev-stack.py`. It declares
    `owner-claude-subscription@1`: `kind subscription-login`, revision 1,
    `delivery_mode server_material_v1`, `material_store
    /var/lib/aq/login-material` and `allowed_node_ids = []`. The empty list is
    deliberate: nothing could lease the login to a sandbox yet.
  - **Import.** `scripts/ms1-demo.sh import-login` pipes the token over stdin
    into an owner-only file on the server's private source volume. It then runs
    `aq-server import-login --credential owner-claude-subscription@1 --revision 1
    --source <staged file>` inside the server container and deletes the staged
    copy.
  - **Result.** Stored version **1**, `origin operator`, 108 bytes, content
    digest `25e32ec85fc9…`, in a 0700 store with a 0600 file owned by the server
    user.
  - **Leak check.** A token grep over the server logs, the controller log and
    the demo logs found 0 files.
  - **Two C5b integration findings** (worked around in the demo; follow-ups
    for the C5b/C6b owners):
    1. **`import-login` cannot be the first import.** `operator_replace`
       requires a fingerprint of the configured `secret_file` (version 0),
       otherwise it reports "no enabled subscription-login credential". The
       demo therefore stages the same token as the configured base file and
       recreates the server before importing. A login can't enter the vault
       through `import-login` alone, contrary to section 7's "operator import".
    2. **Trailing newline.** `import-login` stores the file bytes verbatim,
       including the newline that `claude setup-token > file` leaves. C6b's
       runner install accepts only ASCII-graphic Claude tokens, so such a login
       would later fail to install. The demo strips `\r\n` before staging.
       The server could reject or trim the newline for Claude logins.
  - **No rotation mid-attempt.** No attempt exists. `import-login` refuses to
    store a second version unless given `--replace`, because an operator replace
    makes running attempts get 409 at their next renewal.
- No API key was used or configured anywhere.

## Per-step results

### Step 1: two dependent tasks; Claude edits, checks and publishes

**Through the stack: not done.** **Node level: done.**

Graph (aq-ms1 workspace `e0000000-0000-4000-8000-000000000001`), change set
`20cb4e7f-371c-4369-8625-9e64f867a8f9`, [graph-seed.json](MS1-evidence/graph-seed-incarnation1.json):

| Task | ID | State | Graph version | Ready |
| --- | --- | --- | --- | --- |
| A: add a hello function to the scratch crate | `e0a20000-0000-4000-8000-000000000011` | active | 2 | yes |
| B: document hello in the README | `e0a20000-0000-4000-8000-000000000012` | active | 2 | no (blocked by A, edge `ad1ac275-be68-42bb-a097-8deae45c81ca`) |

Seed ([e0-seed.json](MS1-evidence/e0-seed.json)):

- repository `e0000000-0000-4000-8000-000000000003` is registered in central aq-git
  for `https://github.com/ElectricJack/aq-scratch.git` `main`;
- project `e0000000-0000-4000-8000-000000000002`;
- settlement namespace `cb0ca531-c218-4729-8f84-d46c0e7e8333`.

Node `ms1-desktop` = `4eb8af55-54a4-4220-a487-9dda14fa3efa` is enrolled and
`online`, with boot IDs `311a8d81…`, then `1a45ad33…` and `63345949…` after
restarts. Every boot logs
`B2 registration/issuance provider is not installed; admission fenced; foundation heartbeat remains active`
([controller.log](MS1-evidence/controller.log)). Task A therefore stays ready and
unclaimed ([gaps.txt](MS1-evidence/gaps.txt)). No task, attempt, grant or permit
ID exists to record.

Node-level real run (`claude_live`, run `run-20261105T052127Z`,
[step1/](MS1-evidence/step1/), [step1.log](MS1-evidence/step1.log)):

- The controller path was `register → reserve` (harness `claude-code`, CLI
  `2.1.318`, template digest above) `→ accept → launch` (create, deliver
  material, start runner) `→ stop → destroy`.
- Claude session `48c1161e-9d54-4c3c-acb8-f4080cc13030`: 3 turns, 4.9 s, `success`,
  `is_error false`, $0.024 list cost. It used `Edit` on `src/lib.rs` only.
- The baseline `cargo test` failed (red). After the edit the check passed
  (`check_rc 0`). The commit was published to a bare repo inside the sandbox
  (`publish_rc 0`): base `d58c454a6ca437516d86ee962ed8f52556dc1a1f` → head
  `21d5967354796131cbff712572788fc100f89733`, published = head.
- Inside the sandbox: `uid 0` (single-UID Podman, see I4a), `CapEff 0`,
  `NoNewPrivs 1`, material wiped, no token on disk, no credentials file.
- Runner `Exited(0)` after about 10 s. Stop was confirmed with the volume
  preserved, destroy was confirmed, and no owned sandboxes were left.

What this does not show: aq-git publication policy (D1a/D1b), a publication
receipt or PR, a retained source revision, or a sandbox ID bound to a server
grant.

### Step 2: automatic completion releases the next task

**Not done.** It needs a settled result from step 1. Task B is still blocked at
graph version 2 and no outbox event exists. D3's default completion path is
covered only by its own component tests (D3.md).

### Step 3: cancel mid-run (B3)

**Through the stack: not done** (no attempt exists, so there is no cancellation ID
or disposition). **Node level: done** with a real Claude session
(`ms1_cancel_mid_claude_run_stops_with_proof`,
[node/cancel/cancel.json](MS1-evidence/node/cancel/cancel.json)):

- attempt `c079bb51-e9aa-4a74-9645-79c480565d56`, sandbox
  `aq-02c0c7483e314b0f9b73f7a97e08b951-c079bb51e9aa4a74964579c480565d56`;
- Claude's first model turn came 4.3 s after launch. The stop was issued while
  Claude was writing (6 of 20 `Write` calls done) and confirmed in 6.0 s with
  proof `StoppedNoProcesses`. Runner exit was `Exited(137)`; Claude never reached
  its own exit;
- the work volume was present after the stop (preserved), the material was
  absent, destroy was confirmed and no owned sandboxes were left.

This is an operator stop. The server cancellation route
(`POST /api/execution/attempts/{id}/cancel` → node poll → controller) was not
exercised.

### Step 4: fail an attempt and see the retry (B4)

**Not done.** Replacement, retry budgets and sandbox-minute budgets are server
decisions (`profiled_execution/replacement.rs`) that need a started attempt. A
node-level "second sandbox" would prove nothing about B4, so none was staged.

### Step 5: restart the worker node, and separately aq-server, mid-attempt

**Through the stack: not done** (no attempt, so no re-adoption, renewal or
duplicate-effect evidence). Measured instead:

- **Node controller restart mid-Claude-run**
  (`ms1_node_restart_mid_claude_run_preserves_without_relaunch`,
  [node/node-restart/node-restart.json](MS1-evidence/node/node-restart/node-restart.json)):
  - attempt `8d8122e2-2411-4751-b8f9-fc4d9d51b7c3`, sandbox
    `aq-863c479cc0d44dcd889c0d7310db6e7a-8d8122e224114751b8f9fc4d9d51b7c3`;
  - the controller was dropped without stop or drain while Claude was running;
  - a new incarnation was opened on the same journal (boot `ca24fccc…` →
    `c31a2b8a…`), and reconcile took 5.9 s;
  - with no server re-adoption answer, the old-boot attempt was marked
    `NoCurrentGrant` and **stopped and preserved** (`StoppedNoProcesses`, volume
    present);
  - **owned sandboxes before = after = 1, with the same backend ID: no second
    create, no relaunch**;
  - destroy was refused 5 times during the 25 s cleanup grace (preserve before
    destroy), then confirmed.

  This is the stop-and-preserve branch of Review Focus 4 on the node side. The
  re-adopt-and-continue branch needs the server's B1 re-adoption through a real
  provider, which does not exist.
- **aq-server recreated under the running controller**
  ([server-restart.log](MS1-evidence/server-restart.log)):
  - `docker compose up --force-recreate server` ran from 05:25:21Z to 05:25:32Z;
  - controller pid 2157025 kept running with the same boot; the node was `online`
    again at 05:25:46Z;
  - no sandbox existed or was created.

  This covers the heartbeat foundation only. The real renewal-window behaviour
  is covered only by B2's `server_restart_mid_attempt_resumes_or_preserves`.
- The server was also recreated twice during setup, by the seed's settlement
  step and by the dashboard overlay. The controller restarted twice under
  `node` (boots above), with the same result.

### Step 6: Windows suspend drill

**Not run by the agent** (it requires the owner at the machine). Procedure
[below](#step-6-windows-suspend-drill-owner-procedure). The helper test was
dry-run without a suspend: one 100-file Claude run to completion. See
[Verification](#verification).

## Missing integration

These are the concrete reasons steps 1–5 cannot run through the stack. Each one
is larger than a small wiring fix (most of all, the first two are the product's
central worker composition), so none was attempted here.

1. **The node controller has no server transport (aq-worker).**
   - `crates/aq-worker/src/main.rs:700` builds the production controller with
     `UnavailableProvider`.
   - `controller/service.rs:61–84` makes `register` and `issue` return
     `ProviderUnavailable`.
   - `service.rs:114–116` states: "B2 will compose native offer/acceptance
     observations with reserve/accept_with/launch. There is deliberately no
     guessed polling or issuance route in this service."

   Missing:
   - a production `PermitProvider` over aq-server's worker API (register,
     offer poll, accept with delegated-permit handoff, `readopt`, `poll` for
     cancellation, `retain`, `emit`);
   - the service loop that turns an accepted offer into
     `reserve → accept_with → launch`.

   The only `PermitProvider` implementations on `main` are test fakes and I4a's
   `Lease`.
2. **The in-sandbox runner never launches (aq-worker).** `main.rs:423–441`,
   `sandbox-run`, validates the frozen manifest and permit transport, then bails
   with "sandbox runner B2 receipt codec is unavailable; permit transport
   validated, no launch attempted".

   The launch pieces exist as library code, but nothing in the binary calls
   them:
   - `native::runner::sandbox::run_with_login` (`sandbox.rs:240`, C6b wiring from
     `calm-delta-18.10.1`);
   - the Claude recipe (`native/adapter/claude.rs`, C6a);
   - hydration (B6c);
   - output publication.

   `calm-delta-18.9.1` ("Wire C6a harness homes into sandbox launch") is
   **PAUSED**.
3. **The server has no production graph-to-v2-offer path (aq-server).**
   - `/v2/worker/poll` offers only synthetic dev fixtures
     (`execution/api.rs:262–287`, "synthetic assignments are development-only").
   - `execution.rs:447` notes that "Production graph scheduling must supply
     checked immutable inputs here".

   Graph tasks reach workers only through the profiled native path
   (`/v1/worker/profiled/*`, the MS0 path). That path needs an `[intelligence]`
   configuration (profiles, credentials, catalog, node binding) which E0's dev
   seed does not create, and its only worker consumer is `run-native`. That
   consumer is in-process (not docker-local) and its installed executor is Codex.
   The in-sandbox runner's routes are the profiled ones
   (`runner/sandbox/auth.rs:68–81`), so the server side is meant to be the
   profiled path. The binding from a docker-local node's offer to a profiled
   attempt with a delegated permit (B2 handoff, C5a2 lease binding) is
   implemented server-side but has no worker consumer.
4. **Claude login lease (aq-server C5b, now merged).** The login is stored and
   versioned (see [Credential handling](#credential-handling)). It still has no
   path to a sandbox, because of items 1–3. The two C5b findings recorded
   there need fixing before a real lease works.
5. **Templates are unadmitted.** The C3c scanner refuses on this host
   (GnuTLS), so the docker-local allowlist here names an unadmitted digest (as
   in I4a).
6. **Single-UID sandbox daemon.** The agent runs as in-container uid 0 (no
   capabilities, no-new-privs), not the `agent` user. This is I4a's recorded
   owner fix (`uidmap`, restart without `AQ_SANDBOX_SINGLE_UID`).

**Ownership.** No open v1 task owns items 1–3. The closest closed owners are B2
(`noble-summit-37.2`, server permits), B5a/B5b (`noble-summit-37.5/.6`, the
controller "provider seam") and B6a1/B6a2 (`noble-summit-37.11/.12`, the
runner). I propose three new tasks:

- **(aq-worker)** a production B2 `PermitProvider` and the offer → launch
  service loop in `aq-worker run`, with a live test against compose.dev;
- **(aq-worker)** `sandbox-run` launch composition: the receipt codec, hydrate
  → harness home → subscription login → Claude recipe → publish → result and
  stopped reports. This continues `calm-delta-18.9.1`;
- **(aq-server)** an E0/dev seed that configures profiled execution for a
  docker-local node (profiles, catalog, node binding). The credential half now
  exists as `AQ_DEV_INTELLIGENCE_FILE` and `import-login`.

Items 4–6 belong to C5b (its two findings), C3c and the I4a owner fix. Following the operator's
instruction to treat agent-queue2 as read-only, this agent filed no v1 tasks.
The operator should file the three tasks above.

## Memory and CPU

These were sampled by `scripts/ms1-stats.sh` during run
`run-20261105T052127Z`. The stack uses `docker stats` every 5 s, the sandboxes
use cgroup `memory.peak` and `cpu.stat` every 1 s, and the node controller uses
`VmRSS`. Raw CSVs: [stats-stack.csv](MS1-evidence/stats-stack.csv),
[stats-sandbox.csv](MS1-evidence/stats-sandbox.csv),
[stats-node.csv](MS1-evidence/stats-node.csv). Summary:
[stats-summary.txt](MS1-evidence/stats-summary.txt).

| Component | Peak memory | Max CPU |
| --- | --- | --- |
| postgres | 134.3 MiB (cap 768 MiB) | 40.6% (during seed and recreate) |
| aq-server | 18.2 MiB (cap 512 MiB) | 4.9% |
| aq-git | 19.4 MiB (cap 256 MiB) | 3.3% |
| aq-graph | 3.8 MiB (cap 256 MiB) | 3.1% |
| **Stack total** (sum per sample) | **172.7 MiB** | — |
| Node controller (`aq-worker run`) | 26.5 MiB RSS | — |
| Sandbox, step 1 (Claude + `cargo test` on a one-file crate) | 531.4 MiB (cgroup peak; cap 4 GiB) | about 30% of one core averaged over the run |
| Sandbox, cancel and restart runs (Claude only, first turns) | 145.3 / 142.7 MiB | 11–14% of one core |
| Sandbox, drill dry run (Claude only, 100 sequential writes, 170 s) | 199.8 MiB | 4.3% of one core averaged over 140 s ([drill-dryrun/](MS1-evidence/drill-dryrun/)) |

Further figures are in [Verification](#verification).

**For E5 sizing:**

- The idle-to-light stack is under 200 MiB.
- A Claude sandbox needs about 150 MiB for the CLI and up to about 0.5 GiB
  with a trivial Rust build.
- These runs do **not** measure a realistic Rust build: the crates are
  one-file. The plan's "two concurrent Rust-building sandboxes" figure still
  needs a real-project build to measure.
- The aq-server sample is from a stack that never ran an attempt.

## Step 6: Windows suspend drill (owner procedure)

The full drill needs the end-to-end path. Until that exists, this interim
version exercises the same WSL2 clock and network behaviour against the real
sandbox, the real Claude session and the running node controller.

1. Make sure the stack and controller are up (`scripts/ms1-demo.sh status`
   shows `ms1-desktop online` and 4 healthy containers). Note the time
   (`date -u`).
2. Run `scripts/ms1-demo.sh drill`. When it prints
   `drill: sandbox aq-… running; suspend Windows now`, choose Start → Power →
   Sleep at once. The run lasts about 3 min if it is not interrupted.
3. Wait at least 60 s, then wake the machine and log in. Do not touch the WSL
   terminal until the drill finishes. It keeps polling every 5 s and prints
   `drill: wall-clock gap of N ms detected` after wake.
4. Record the results:
   - **Timings:** `suspend_windows` (wall and monotonic gap) and `elapsed_ms`,
     in `~/.local/share/aq-ms1/evidence/drill-*/suspend-drill/suspend-drill.json`.
   - **Did the attempt continue?** `runner_exit` (`Exited(0)` means the sandbox
     survived and finished), `claude_rc` and `write_calls` (100 means no lost
     work; fewer with exit 0 means Claude stopped early; read `claude.jsonl`).
   - **Reconcile log:** `drill-*/controller.log` (look for reconnect and
     heartbeat warnings around the gap) and `drill-*/server.log`.
   - **After wake:** `scripts/ms1-demo.sh status` shows `ms1-desktop online` with
     a fresh `last_seen_at`, and no sandboxes are left behind (`sandboxes on … : 0`).
5. Once the end-to-end path exists, repeat with a real graph attempt. Record the
   attempt's renewal and deadline, and whether the server kept the grant
   (continue) or replaced the attempt in a new sandbox (retry).

## What was changed

No production Rust code was changed. The one integration fix is in the dev
tooling: `scripts/dev-stack.py` gains an opt-in `AQ_DEV_INTELLIGENCE_FILE`, a
non-secret TOML file holding only `[intelligence]` tables. It is appended to the
generated dev vault, so E0 can carry a subscription-login credential for
`import-login`. It is tested by `scripts/test-dev-stack.py::test_prepare_appends_only_an_operator_intelligence_section`:
the file is appended, replay is stable, dropping it is refused, other tables,
non-TOML and symlinks are refused. No other defect small enough to fix inline
was found on the reachable path.

- **aq-worker**, branch `ms1/demo`: `crates/aq-worker/tests/ms1_live.rs`, with
  these tests:
  - one pure test, `long_runner_carries_no_secret_and_wipes_the_material_first`;
  - three ignored live tests: `ms1_cancel_mid_claude_run_stops_with_proof`,
    `ms1_node_restart_mid_claude_run_preserves_without_relaunch` and
    `ms1_suspend_drill_long_claude_run`.

  They reuse I4a's `tests/support/docker_live.rs`.
- **aq-server**, branch `ms1/demo`:
  - `scripts/ms1-demo.sh` and `scripts/ms1-stats.sh`;
  - `scripts/dev-stack.py` (`AQ_DEV_INTELLIGENCE_FILE`) and its test in
    `scripts/test-dev-stack.py`;
  - this document;
  - `docs/implementation/MS1-evidence/`.

  No migrations.

Operator notes found while standing the stack up (documented, not code
defects):

- **The controller root must not be pre-created.** An existing directory with
  mode 0755 gives `controller root changed`; an existing empty directory gives
  `controller file open failed`. The demo lets the controller create it.
- **`dev-stack.py seed` drops the dashboard overlay.** Its settlement step
  recreates the server with only `compose.yaml` and `compose.dev.yaml`, so the
  demo recreates the server again with `compose.dashboard.yaml`.

## Verification

- `cargo test --locked -p aq-worker --test claude_live live -- --ignored`:
  1 passed, run twice (05:08 and 05:21 UTC).
- `cargo test --locked -p aq-worker --test ms1_live ms1_ -- --ignored --test-threads=1`:
  2 passed, at 05:11 and 05:22 UTC. The first attempt at 05:09 failed on a test
  defect: it destroyed inside the cleanup grace. That run's sandbox was removed
  by hand; the test now waits out the grace and records the refusals.
- `cargo test --locked -p aq-worker --test ms1_live ms1_suspend_drill -- --ignored`
  (dry run, no suspend):
  - 20-file version: passed, 37.9 s, `Exited(0)`, 20 writes;
  - 100-file version: passed, 170.3 s, `Exited(0)`, `claude_rc 0`, 100 writes,
    no suspend windows ([suspend-drill.json](MS1-evidence/drill-dryrun/suspend-drill.json)).
- `python3 -m pytest -q scripts/test-dev-stack.py`: 13 passed. The new test
  failed first (red) and then passed.
- `scripts/ms1-demo.sh import-login` against incarnation 3: stored version 1.
  An earlier run against incarnation 2 failed with "no enabled subscription-login
  credential" (C5b finding 1).
- `cargo test --locked -p aq-worker --test ms1_live`: 1 passed, 3 ignored.
- `cargo clippy --locked -p aq-worker --test ms1_live -- -D warnings`: clean.
- `rustfmt --edition 2024 --check crates/aq-worker/tests/ms1_live.rs`: clean.
- `bash -n` over `scripts/ms1-demo.sh` and `scripts/ms1-stats.sh`. Every demo
  subcommand above except `all`, `down` and `drill` was run against the live
  stack.
- The token grep over `MS1-evidence/` found 0 files.
- Cargo settings: `CARGO_TARGET_DIR=…/target-ms1`, `RUST_TEST_THREADS` 1 for the
  live tests and 2 otherwise.

## Limits

- MS1's acceptance (owner approval after watching or rerunning) cannot be met
  until the gaps above are closed. The node-level runs are evidence for the
  sandbox, credential and recovery layers only. They are not substitutes for
  steps 1–5.
- The node-level runs publish to a bare repo inside the sandbox, not through
  aq-git, and use I4a's fixed runner, not the C6a/B7 runner with the scoped
  `aq-agent`.
- The aq-server image and the worker binary are local builds with no registry
  digests. The template is unadmitted.
- The memory figures come from trivial workloads (see [Memory and CPU](#memory-and-cpu)).
- The `aq-ms1` stack is left running for the owner.

Owner verification: pending

## Refreshing the context measurement (every 24 hours)

The profiled configuration carries a context qualification built from three
bounded Claude calls (`~/.local/share/aq-ms1/measure`). It expires 24 hours
after `measure/observed_at_ms` (`QUALIFICATION_TTL_MS` in
`scripts/ms1-profiled.py`). After that, `context_budget::evaluate` refuses
every candidate, every profiled poll that would carry an offer gets HTTP 409,
and the stack makes no offers. Until spec 0007 phase 1 automates it, someone
re-measures before expiry:

```sh
~/.local/share/aq-ms1/remeasure.sh check   # what the running server loaded, and its expiry
~/.local/share/aq-ms1/remeasure.sh         # re-measure, bump the revision, `up.sh profiled`, check
~/.local/share/aq-ms1/remeasure.sh measure # the three calls and validation only; installs nothing
```

`remeasure.sh` does what the 0006 P6 proof did (aq-worker
`docs/implementation/0006-P6.md`, "Re-measurement"):

1. **Preflight.** The token file is a 0600 regular file. The server is
   healthy. No MS1 attempt or sandbox is running, because the server is
   recreated. A rebuild of the current revision must equal `configure.json`,
   so the new revision differs only by the measurement
   (`AQ_MS1_ALLOW_DRIFT=1` overrides this check).
2. **Measure.** One `docker -H unix:///run/user/1000/aq-sandbox/podman.sock
   run --rm -i --pids-limit 512` of the node's template
   (`worker/node.json`, `docker_local.templates[0]`) runs the three calls.
   - Each call: `/opt/aq/bin/claude -p --output-format stream-json --verbose
     --input-format text --model claude-sonnet-5-5 --permission-mode
     bypassPermissions --setting-sources user --settings … --strict-mcp-config
     --no-session-persistence --disable-slash-commands < PROMPT`.
   - The env and settings are the runner's: `HOME`/`CLAUDE_CONFIG_DIR=/home/agent/.aq/claude`,
     telemetry, autoupdate and auto memory off, cwd `/aq/work`.
   - `observed_at_ms` is taken in the sandbox just before the calls.
   - The token file goes to the container on stdin. It becomes
     `CLAUDE_CODE_OAUTH_TOKEN` of each claude process only. It is never in
     argv, a container's env or config, a copied file or a log.
3. **Validate.** `ms1-profiled.py`'s own `measured()` checks every call:
   success, model `claude-sonnet-5-5`, a single context window, and total
   input within the counting bound. The output files are also scanned for the
   token.
4. **Install.** Every overwritten file is first copied to `*.bak-YYYYMMDD`:
   - `measure/*.jsonl` and `observed_at_ms`;
   - `revision`, `intelligence.toml`, `configure.json` and `catalog-rev2.json`;
   - `node/config/*.json`;
   - `evidence/profiled-*.json`.

   Then the script installs the new outputs and bumps `revision` by one. A
   changed body needs a new version.
5. **Apply.** `up.sh profiled` writes the configuration, recreates the server
   on it, and activates the catalog and the project binding.
6. **Check.** The script reads `[profiled_execution]` back from the server's
   vault file and prints the revision, `observed_at_ms` and `expires_at_ms`.
   No server read reports the qualification itself. The eligibility read
   answers `node_not_eligible` while no node runs, before and after.

The log is at `evidence/remeasure-<UTC>.log`.

First run, 2026-11-07:
- Revision 16 to 17, catalog revision 8, project binding revision 7.
- `observed_at_ms` 1794089390110 (22:09:50Z).
- Expires 2026-11-08T22:09:50Z.
