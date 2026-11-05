# MS0 — first real agent run gate

Task `stark-zenith-16.1`. Evidence was re-evaluated on 2026-11-03 at 18:53 UTC,
after the operator reopened the gate.

**Gate passes on the owner's decision.** On 2026-11-03, one real Codex run
completed a graph task end to end: I0 run 13c.

- Codex 0.166.0 ran on the owner's ChatGPT login, with no API key.
- It made the requested one-line README edit. The edit was retained centrally as
  checkpoint `17fe8a72` over base `2e32645`.
- It settled through the human-review route: owner approval, evaluation accepted,
  completion confirmed. Graph task `…0007` is now `completed`.
- The login was not modified.

MS0's checklist asks for **three** such completions (plan §1, and the owner's
pre-approval). The evidence has **one**. The owner accepted one settled run as
passing MS0: its purpose is met, and two more runs would spend Codex usage that
stood at 97% of the weekly allowance. The decision is recorded
[verbatim below](#owner-decision).

No MS1 contract or dependency was changed.

## Owner decision

### Standing instruction, verbatim

Owner `user:local`, 2026-11-02, task comment
`comment-626f0ab3daf0494aa56f0074ddb52482`:

> Owner pre-approved MS0 on 2026-11-02 before stepping away. Verification switched from human to qa_agent: close MS0 when I0's evidence shows three real Codex API-key runs with retained source and recorded outcomes, and every problem found is filed as a task. If I0 could not complete (for example no API key or scratch repository was provided), do not pass MS0; close it fail with what is missing.

### Reopen instruction, verbatim

Operator `human:local-operator`, 2026-11-03 reopen feedback:

> Operator: the first real agent run now SUCCEEDED. The I0 re-run calm-stone-85 (aq-worker) completed run 13c end to end on aq-server 2d337f36: a real Codex 0.166.0 run with the owner's ChatGPT login in the docker-local sandbox on aq-e0-qual made a 1-line README edit, retained as checkpoint 17fe8a72 over base 2e32645; the owner MS0 settlement approved, evaluation d58ab4f8 accepted, completion b196d43a reconcile confirmed, holds_task false, and the login was not mutated. Evidence: aq-worker branch aq/calm-stone-85, docs/implementation/evidence/i0-first-real-run/run13.json. Re-evaluate the MS0 gate against its checklist using that evidence, update docs/implementation/MS0.md, and close pass if met. Record the three run-13 findings (session held to the wall limit after submit; 2 s selection TTL race; stale attempt state after completion) as follow-ups, not blockers. Velocity rules apply.

### Decision on the run count, verbatim

Owner `user:local`, 2026-11-03, task comment
`comment-d0f878b620544288a6875d029b9ade96`, in reply to message
`msg-89e92e67ec8a4d84919a8969ee4defc3`:

> Operator decision recorded in msg to task: accept 1 settled run for MS0 (owner pre-approval, Codex budget); ChatGPT-login wording supersedes API-key; MS1 rows become follow-up tasks.

The message it refers to, `msg-c67225924eda4d0c9f2a9b1ec81d27ee` (from
`user:cli`, to this task), verbatim:

> Operator decision (owner-delegated; the owner pre-approved the MS0 gate on 2026-11-02 and can override):
> (1) ACCEPT the one fully settled run (I0 run 13c) as passing MS0. MS0's purpose is met, and two more settled runs would spend Codex usage that is at 97% weekly. Record the 1-of-3 deviation and this rationale in MS0.md.
> (2) Confirmed: plan §1's ChatGPT-login wording supersedes the 'API-key' wording in the pre-approval comment. The owner uses subscription logins only, never API keys.
> (3) For each MS1 'Yes' row in your problem table, make sure a follow-up task exists in the owning project: session held to wall limit (C6a, aq-worker), offer window (quick-impact-98 plus the aq-worker deadline anchoring), stopped attempts holding tasks (B4/B7/D3), login rotation (C5b/C6b). Do not change contracts in this task.
> Then close MS0 pass.

This answers the [decision questions](#decision-questions):

1. **Run count: one settled run passes MS0.** This is a deviation: the checklist
   asks for three runs and the evidence has one. The recorded rationale is that
   MS0's purpose (surface the real problems before MS1) is met, and two more
   settled runs would spend Codex usage that stood at 97% of the weekly
   allowance. The owner can override.
2. **Authentication:** the ChatGPT-login wording supersedes "API key". The owner
   uses subscription logins only.
3. **MS1 scope:** the MS1-impact rows become follow-up tasks. No MS1 contract is
   edited by this gate. Follow-ups for the four named rows:

   | Row | Owning project | Follow-up |
   | --- | --- | --- |
   | Session held to the wall limit (C6a) | aq-worker | `CROSS-REPO FOLLOW-UP (aq-worker)` comment on this task. This session is scoped to aq-platform and cannot create aq-worker tasks, so the supervisor routes the comment into an aq-worker task |
   | Offer window (B2/C6a) | aq-platform + aq-worker | `quick-impact-98` (server; completed). The worker's deadline anchoring (poll latency counted twice) is a second `CROSS-REPO FOLLOW-UP (aq-worker)` comment, routed the same way |
   | Stopped attempts holding tasks (B4/B7/D3) | aq-platform | `calm-beacon-15` |
   | Login rotation (C5b/C6b) | aq-worker | Existing `fresh-delta-13` |

   The other "Yes" rows already map to existing v1 tasks: `sharp-harbor-18`,
   `amber-horizon-91` and `sharp-dune-88`. Stale attempt state is `bold-delta-91`.

## Checklist against the evidence

| MS0 requirement | Evidence | Met? |
| --- | --- | --- |
| Codex CLI on the owner's ChatGPT login, no API key (plan §1) | Profile `owner-chatgpt-codex`, Codex 0.166.0, `gpt-6.1-sol` high. Login delivered by material lease. Owner `auth.json`, leased material and vault copy all byte-identical to baseline (I0 report) | Yes |
| Plain container on the desktop, today's native runner, local full stack | `aq-worker run-native` in container `aq-i0-run13` against `aq-e0-qual` on WSL2 | Yes |
| Completes a real graph task | Run 13c: graph task `e0a10000-0000-4000-8000-000000000007` completed (version 2), cross-checked below | Yes, once |
| … three times | One settled completion (13c). Run 12 retained source but no change and no settlement. Runs 9–11 retained nothing | 1/3; **owner accepted 1** |
| Retained source and recorded outcomes | 13c: transfer `df5a8007…` retained, checkpoint `17fe8a72`; report `095807a2…`; evaluation accepted, completion confirmed | Yes, for 13c |
| Problems found are filed as tasks | Run-13 server findings filed as `quick-impact-98` and `bold-delta-91`; worker findings recorded as cross-repo follow-ups on this task. Earlier server findings are only partly traceable to IDs (see the problem table) | Mostly |
| Owner API-key wording in the 2026-11-02 comment | Superseded by plan §1's ChatGPT-login wording, which I0 followed | Yes: owner confirmed |

## Evidence and provenance

The evidence is the aq-worker I0 report at commit
`29e23f70b597696ac58b6bfab802a4da33446158` ("run 13 settled end to end"). That
commit is on `origin/aq/calm-stone-85` and also in `origin/main`. Frozen copies
are in [`evidence/MS0/run13/`](evidence/MS0/run13/), so workspace recycling
cannot lose them:

- [I0 report](evidence/MS0/run13/I0-first-real-run.md)
- [run13.json](evidence/MS0/run13/run13.json)
- [run12.json](evidence/MS0/run13/run12.json)

Their SHA-256 values are in [observations.json](evidence/MS0/run13/observations.json).
The morning review's frozen copy, [evidence/MS0/I0-first-real-run.md](evidence/MS0/I0-first-real-run.md),
is kept as history.

| Component | Run 13c value |
| --- | --- |
| aq-server source / image | `2d337f368ffcfc71c5de929a38fbacaa3b0c5d75` (PR #50, `keen-impact-58`) / `sha256:f7f726c88654c7b16494f0a4bee71425c25ddffa04c18b4baa86a5cac800cb6b` |
| aq-worker image / source | `sha256:20abe6352e6a1161a70ce56ee89239487a2d50f3235701088862c131a64db389` / `f4703fec23b1d7d0b4cb7bd2a9edf46031a7b5cd`. Contains the `azure-horizon-22.1.1`–`.4` fixes |
| Codex / model | 0.166.0 / `gpt-6.1-sol`, native reasoning high |
| Graph / Git support images | `sha256:807beba98dcd…` / `sha256:94a3d9adea2c…`, unchanged from E0 refresh |
| Definitions | `worker_i0.definition_version` "3", profiled revision 6, intelligence revision 4 |
| Node / profile | `f761410b-43f5-4106-ab80-f044dd446da0` / `owner-chatgpt-codex` |
| Target | `ElectricJack/aq-scratch` `main`, repository `e0a10000-0000-4000-8000-000000000003`, base `2e32645bab743729bfaeb0f2eec417c2ace7794b` |

Like the earlier image, the worker image is a local build. No registry RepoDigest
is claimed.

## Run-by-run audit

The table covers every I0 invocation that reached admission. The earlier
invocations 9 and 10 are covered in the [morning review](#earlier-evaluation-0-of-3).

| Invocation | Attempt ID | Grant ID | Graph task ID | Retained source revision | Settlement outcome / route |
| --- | --- | --- | --- | --- | --- |
| 11 (`azure-horizon-22.1`) | `87d16454-c2cc-593c-9bfd-60ac4ea56911` | `d56e197c-3955-4dd0-bc52-2016724b7e58` | `…0005` | None | Exit 2 after 944 s; companion tools missing; no settlement |
| 12 (`calm-stone-85`) | `ab352201-950e-5a77-9008-a374c85f2f00` | `53c38069…` (prefix only in report) | `…0006` | Handle `1640c116…`, checkpoint equals base `2e32645` (no change) | Report `7439f3bc…` `expired`. Settlement routes 404 on server `61b8d0e5`; not settled |
| 13a / 13b | `aee8e9ed…` / `613a516e…` | None | `…0007` (not leased) | None | Exit 2 in under 3 s: lost the offer window; Codex not launched |
| **13c** | **`f246a730-1fcd-5c62-a4a2-d98f4851eaab`** | **`53437605-fcfa-44cb-9027-5d5217021c5a`** | **`e0a10000-0000-4000-8000-000000000007`** | **`17fe8a728b05d97963b86116a1543f54d0785db8`** (parent `2e32645`, `README.md \| 1 +`) | **Human-review route: approve → evaluation `d58ab4f8…` accepted → completion `b196d43a…` confirmed** |

### Run 13c, end to end

- **Run.** Container `aq-i0-run13`, 18:16:03Z to 18:31:46Z, 941 s, exit 0.
  Offer `dce3972e-3802-599b-bfcb-0c553595c2ac`, assignment generation 3, session
  `0c695d7d-cfff-5db2-b502-3a679a09d423`. Every worker route in the run window
  answered 200, including 423 terminal-relay uploads; run 12 had 214 × 409.
- **Work.** Codex appended exactly `I0 run 13 (calm-stone-85): hello from aq-worker`
  to `README.md`. It checked the bytes and `git diff --check`, and submitted its
  in-session result at 18:16:38Z, 35 s after launch.
- **Report.** `095807a2-9dc3-4a16-be5a-9abbb26c2730`, process outcome `expired`.
  The session was held until the 900 s wall limit, a follow-up finding below.
  The in-session conclusion is recorded unchanged.
- **Retained source.** Transfer `df5a8007-6e5c-41ad-851a-6f0df3a287ac`, source receipt
  `74145d1f…`, import manifest `ec1c290c…`, checkpoint `17fe8a72` with single
  parent `2e32645`.
- **Settlement.** Done by the owner MS0 reviewer `7f23ea6c…` with the dev owner
  identity at `:32848`:

  | Step | Route | Result |
  | --- | --- | --- |
  | Prepare | `POST /api/reviews/combined/prepare` | 200, subject `aq.source-effects-review-subject/095807a2…` |
  | Admit | `POST /api/reviews/combined` | 200, decision `2c66a3c3-431f-4f3a-8315-dfef70b77892` |
  | Inspect | `GET /api/reviews/combined/{id}` | 200, pending, revision 1 |
  | Evaluate early | `POST /api/settlement/evaluations` | 503 `updates_unavailable` (no answer yet), as expected |
  | Resolve | `POST /api/reviews/combined/{id}/resolve` | 200, **approve**, revision 2, `human_answer` |
  | Evaluate | Same intent, retried | 200, `d58ab4f8-3fcc-48cc-898e-8be679885b64` **accepted**, release `c821499f…` |
  | Reconcile | `POST /api/settlement/completions/b196d43a…/reconcile` | 200, **confirmed**, dispatch 1, graph task version 2 |

- **Login.** Not refreshed, so no writeback was needed. The owner `auth.json`
  (0600, mtime unchanged), the leased material and the vault copy are
  byte-identical to the baseline (sha256 prefix `98a9a084`). MS0 did not read,
  copy or hash the credential itself.

### Independent stack checks

Read-only SQL (`default_transaction_read_only = on`) against `aq-e0-qual` Postgres
at 18:53 UTC. The full record is in [observations.json](evidence/MS0/run13/observations.json).

- All four `aq-e0-qual` services are healthy. The server runs image `f7f726c88654`,
  matching the report.
- **aq_graph:** task `…0007` has `task_state = completed`, version 2, content
  revision 2, updated 18:35:12Z. Tasks `…0004`, `…0005` and `…0006` are still
  `active`, version 1.
- **agent_q:**
  - Attempt `f246a730…`: `holds_task = false`.
  - Source transfer `df5a8007…`: `retained`. Its central receipt names
    `17fe8a728b05d97963b86116a1543f54d0785db8`, retention `active`.
  - Evaluation `d58ab4f8…`: `accepted`.
  - Completion `b196d43a…`: `confirmed`, dispatch 1, payload patch
    `task_state: completed`. Its outbox is a `task.completed` event for report
    `095807a2…`, still unacknowledged.
- **Confirmed findings:**
  - The 13c attempt's `state.status` is still `offered`, stale after settlement.
  - The attempts for runs 9, 11 and 12 (`2d41bbec…`, `87d16454…`, `ab352201…`)
    still hold their graph tasks. Stopped, unsettled attempts are not released.
- **Not checked:** the checkpoint's file bytes. I0 read them from the central
  object store; MS0 relied on the retained receipt OID.
- No admission, terminal input, settlement mutation, restart or provider run was
  performed.

## Problems and proposed MS1 impact

Severity and the MS1 tasks affected are **proposals** for the owner, not
decisions. "Yes" means a contract or qualification clarification may be needed;
it does not edit that task's scope. The status column reflects the run-12 and
run-13 evidence.

### Found by run 13 (new)

| ID | Finding | Proposed severity | MS1 tasks potentially affected | Affects MS1 scope? |
| --- | --- | --- | --- | --- |
| aq-worker cross-repo follow-up (comment on this task) | The native runner holds the Codex session until the wall limit after the in-session result is recorded, so a completed edit reports `expired` after about 15 min | High | C6a, I4a | Yes: MS1's "next task starts automatically" needs prompt session end |
| `quick-impact-98` (server) + aq-worker cross-repo follow-up | The offer window is about 1.4–1.8 s because of the 2 s selection TTL (`aq-selection` `select.rs:238`). The worker also counts poll latency twice. Accept loses under host load (13a, 13b) | High | B2, C6a, I4a | Yes: unattended runs on a loaded desktop fail at random |
| `bold-delta-91` (server) | Attempt state JSON (`offered`, retention `pending`) and the completion `outbox_acknowledged` flag are stale after confirmed settlement | Medium | B4, B7, D3 | Unclear: cosmetic for settlement unless the outbox drives the next task |

### Found by runs 9–12

| v1 task | Finding (I0 label) | Status after run 13 | MS1 tasks potentially affected | Affects MS1 scope? |
| --- | --- | --- | --- | --- |
| `sharp-harbor-18` | Missing real native qualification/configuration inputs | Worked around in `aq-e0-qual` scripts | B2, B6a1, C6a, I4a | Yes: measured inputs must reach the runnable stack |
| `quick-glacier-78` | Human settlement runtime/routes absent | Resolved for qualification by aq-server PR #50 (`2d337f36`) | D3, I4a | No longer; recheck in the D3 contract |
| `fresh-delta-13` | Refreshed ChatGPT login lacks durable owner/server reconciliation | Open; no refresh happened in runs 12–13, so writeback is verified only on synthetic logins | C5b, C6b, I4a | Yes |
| `vivid-bridge-32` | Imported project/supervisor directions exceed live project scope | Open | I4a / operator runbook | No |
| `amber-horizon-91` | Native image lacks current worker, helper, pinned tmux/Codex and libraries | Addressed by worker image `20abe635` for I0 | C1, C3a, C6a, I4a | Yes: image/runtime input contract |
| `stark-grove-17` | Baseline formatting and nested-worktree metadata failures | Open | I4a verification | No |
| `sharp-dune-88` | Default Docker seccomp blocks the real Codex sandbox probe | Open; I0 used a bounded `seccomp=unconfined` exception | C2, C3a, C6a, I4a | Yes |
| `azure-horizon-22.1.1` | Image omits Codex companion executables; tools fail | Fixed in worker `f4703fe` (tools ran in runs 12–13) | C1, C3a | No longer |
| `azure-horizon-22.1.2` | Executor stalls at the workspace trust prompt | Fixed in worker `f4703fe` (no terminal input in runs 12–13) | C6a | No longer |
| `azure-horizon-22.1.3` | Stopped unsettled attempts consume graph tasks | Worker reconcile fixed. Three attempts still hold tasks `…0004`–`…0006` on the server; server release filed as `calm-beacon-15` | B4, B7, D3 | Yes: a server release path for stopped attempts is still missing |
| `azure-horizon-22.1.4` | run-native/check-native hide the failure cause | Fixed in worker `f4703fe` | C6a, I4a | No |
| Server, routed by the supervisor to the consolidated I0 findings task (ID not recorded here) | Relay upload 409 storm without a watcher | Fixed by PR #50: 423 × 200 in 13c | D1b, I4a | No longer |
| Same | Input deadlines over 2 s give a generic 422; opaque relay poll 500s (run 12); opaque 409 from `admission_definition_identity_guard` (run 12) | Not seen in 13c; root causes not confirmed fixed | D1b, B2, I4a | Unclear |
| Same | Missing `[terminals]` validation permits an unusable stack | Open | C6a, I4a | Unclear |

## Decision questions

Sent to the owner; [answered](#owner-decision).

1. **Run count (blocking).** MS0 asks for three completed graph tasks. The evidence
   has one settled end-to-end run (13c). Run 12 also ran real Codex and retained
   source, but made no change and could not be settled. Do you accept one settled
   run as passing MS0, or must I0 complete two more settled runs first?
   *Agent recommendation:* accept, if MS0's purpose (surface the real problems
   before MS1) matters more than the count. I0's own contract asks for one run;
   each extra run costs about 15 minutes and Codex quota while the session-hold
   bug stands; and the new failure modes are filed. Keep three if repeatability
   is the point of the gate. The offer-window race (13a, 13b) suggests it is not
   yet repeatable without the cpu-shares workaround.
2. **Authentication wording.** Confirm that plan §1's ChatGPT-login wording
   supersedes "API-key" in the 2026-11-02 pre-approval comment.
3. **MS1 scope.** Which "Yes" rows need MS1 contract changes? The strongest
   candidates are prompt session end (C6a), the offer window (B2/C6a), the server
   release of stopped attempts (B4/B7/D3) and login rotation (C5b/C6b). MS0 does
   not edit those contracts.

## Demo script and expected observations

This replays one I0 run in the run-13c configuration. It is **not executed by
MS0**; the evidence above is I0's run. Rerunning it spends Codex quota and uses
the owner login through the material lease.

### 1. Bring up E0

From an aq-server checkout at `2d337f36` or later:

```sh
scripts/compose-smoke.sh dev
```

Expected: `compose_dev_reaches_ready_with_graph_and_git` passes. This brings up
and tears down a disposable E0 stack. The run itself uses the retained
qualification stack. Inspect it from aq-worker:

```sh
docker ps --filter name=aq-e0-qual --format '{{.Names}}\t{{.Status}}\t{{.Image}}'
```

Expected: `server` (image `f7f726c88654`), `aq-graph`, `aq-git` and `postgres` all
healthy. The admin port is 32848. If the stack is missing, rebuild it with the
aq-worker `scripts/e0-qual/` helpers and the authorized private inputs, as the I0
report describes. Never use fixture measurements. The node measurement expires
2026-11-04 07:22:45 UTC; re-measure after that.

### 2. Build the I0 worker image

From the aq-worker checkout at `f4703fe` or later, with the I0 build context:

```sh
docker build --progress=plain --target i0 \
  --build-arg AQ_SOURCE_REVISION="$(git rev-parse HEAD)" \
  -t aq-i0-worker:ms0-demo /home/jkern/.local/state/aq-platform-i0/build
docker image inspect --format '{{.Id}}' aq-i0-worker:ms0-demo
```

The build context also takes the archive digest argument named in the I0 report.

Expected: the build completes with Codex 0.166.0 and its companion files. A new
image ID is recorded; it need not equal `20abe635…`. A changed image or
definition body needs a `definition_version` bump in `scripts/e0-qual/definitions.py`.
Otherwise the profiled poll answers 409 (run 12-409).

### 3. Run one native invocation

Seed a fresh graph task with a concrete edit, as for `…0007`. Held tasks cannot
be reused. Then, from the worker checkout:

```sh
: "${MS0_WORKER_IMAGE:?set the immutable worker image ID}"
MS0_RUN_NAME="aq-ms0-demo-$(date +%s)"
MS0_VOLUME="${MS0_RUN_NAME}-state"
MS0_BUNDLE=/home/jkern/.local/state/aq-platform-e0-qual/qual/worker
docker volume create "$MS0_VOLUME"
docker run --rm --network=none --user=0:0 --cap-drop=ALL \
  --cap-add=CHOWN --cap-add=DAC_OVERRIDE --cap-add=FOWNER \
  -v "$MS0_VOLUME:/state" -v "$MS0_BUNDLE:/bundle:ro" \
  --entrypoint=/bin/sh "$MS0_WORKER_IMAGE" -c '
    set -eu; umask 077
    chown 10001:10001 /state; chmod 0700 /state
    for d in config host git; do install -d -o 10001 -g 10001 -m 0700 /state/$d; done
    for f in native.json adapter.json client.json composite.json inventory.json worker.toml; do
      install -o 10001 -g 10001 -m 0600 /bundle/$f /state/config/$f
    done'
docker run --name "$MS0_RUN_NAME" --network=host --user=10001:10001 \
  --cpu-shares=8192 --read-only --tmpfs=/tmp:mode=1777 --cap-drop=ALL \
  --security-opt=no-new-privileges --security-opt=seccomp=unconfined \
  -v "$MS0_VOLUME:/state" \
  -v "$PWD/scripts/e0-qual/i0-run-native.py:/i0-entry.py:ro" \
  --entrypoint=/usr/bin/python3 "$MS0_WORKER_IMAGE" /i0-entry.py
```

The entry starts worker-side aq-git, registers the worker binding, then runs
exactly:

```sh
/opt/aq-runtime/aq-worker --config /state/config/worker.toml \
  run-native --native-config /state/config/native.json
```

Two flags are workarounds, not production settings:

- `--cpu-shares=8192` works around the offer-window race (`quick-impact-98`). Give
  the qualification stack's containers the same shares, and reset all of them to
  1024 afterwards.
- `seccomp=unconfined` is the bounded I0 sandbox exception (`sharp-dune-88`).

Expected:

- Within seconds: admission, a grant and a material lease, then Codex launches. An
  exit 2 with "phase local observation expired" means the accept lost the offer
  window. Retry on a quieter host.
- Within about a minute: Codex makes the edit and submits its in-session result.
- At the 900 s wall limit (until the session-hold follow-up is fixed): the final
  line is `native run: AttemptStopped; retained state preserved`, exit 0. The
  report says `expired`, and the source transfer is retained.

Record the attempt, grant and task IDs and the retained checkpoint OID. Keep the
container and volume on any failure. Check that the owner `auth.json` is unchanged.
If it was refreshed, reconcile it with the I0 writeback helper before disposal.

### 4. Settle through the human-review route

Put the bound values from this run into the request files. They are not fixture
bodies.

- `ms0-prepare.json` names the report.
- `ms0-admit.json` carries the prepared subject reference exactly as prepare
  returned it.
- `ms0-review-answer.json` holds `expected_subject` and `answer`, with the owner's
  own `choice` and `comment`. This script does not approve for the owner.
- `ms0-evaluate-intent.json` holds `schema_version`, `operation_id`, `report_id`,
  `source_operation_id`, `decision_id` and `subject`.

The request types are in `crates/aq-server/src/profiled_execution/settlement/owner.rs`
and `crates/aq-review-protocol/src/lib.rs`. See also the
[GATE-04 route contract](GATE-04-human-settlement-http.md).

```sh
: "${MS0_DECISION_ID:?set after admit}"
: "${MS0_OPERATION_ID:?set the stable evaluation operation ID}"
: "${MS0_COMPLETION_ID:?set from the accepted evaluation receipt}"
MS0_ADMIN=http://127.0.0.1:32848
H=(-H 'X-AQ-Request: 1' -H 'Content-Type: application/json')
curl --fail-with-body -sS "${H[@]}" --data-binary @ms0-prepare.json "$MS0_ADMIN/api/reviews/combined/prepare"
curl --fail-with-body -sS "${H[@]}" --data-binary @ms0-admit.json "$MS0_ADMIN/api/reviews/combined"
curl --fail-with-body -sS "$MS0_ADMIN/api/reviews/combined/$MS0_DECISION_ID"
curl --fail-with-body -sS "${H[@]}" --data-binary @ms0-review-answer.json \
  "$MS0_ADMIN/api/reviews/combined/$MS0_DECISION_ID/resolve"
curl --fail-with-body -sS "${H[@]}" --data-binary @ms0-evaluate-intent.json "$MS0_ADMIN/api/settlement/evaluations"
curl --fail-with-body -sS "$MS0_ADMIN/api/settlement/evaluations/$MS0_OPERATION_ID"
curl --fail-with-body -sS "${H[@]}" --data-binary '{}' \
  "$MS0_ADMIN/api/settlement/completions/$MS0_COMPLETION_ID/reconcile"
```

Expected at each step, matching run 13c:

1. Prepare returns 200 with the subject reference.
2. Admit returns 200 with the decision ID and the subject echoed back.
3. Inspect shows `pending` at revision 1, with the criteria and the recorded result.
4. Resolve returns 200 with the owner's choice at revision 2, finality `human_answer`.
5. Evaluate returns 200 with disposition `accepted`, a release ID and a completion
   operation. An evaluation sent before the answer returns 503
   `updates_unavailable`; retry the same intent afterwards.
6. Reconcile returns 200 `confirmed`. The graph task is `completed` at version 2.

On a conflict or uncertain dispatch, reconcile the original operation. Never
create a new approval or operation. One demo run is one completion, not the
three-run count in question 1.

## Earlier evaluation (0 of 3)

At 10:04 UTC on 2026-11-03, this gate was closed **fail**. I0 (`azure-horizon-22.1`,
report `17bb649`) had 0 of 3 successful runs.

- Run 11 reached real Codex, but the companion tools were missing.
- No source was retained and no settlement route existed.

That record, with its run 9–11 audit, is commit `3440f153` and the frozen copy
[`evidence/MS0/I0-first-real-run.md`](evidence/MS0/I0-first-real-run.md), with
[observations.json](evidence/MS0/observations.json). Runs 12 and 13 superseded
its blockers, as the problem table shows.

## Verification and limits

This is a documentation change only. No Rust, dashboard or wire contract changed.
MS0 has no named test deliverables, so application TDD, Cargo and dashboard
suites do not apply.

- **Checks:**
  - `git diff --check`.
  - JSON parse of `evidence/MS0/run13/*.json`.
  - SHA-256 of the frozen copies, matched against `observations.json`.
  - A local-link check over this file.
  - `bash -n` over the extracted `sh` blocks.
  - A secret-pattern grep over the frozen evidence.
- **Live observations:** read-only SQL on `aq-e0-qual` Postgres (`agent_q` and
  `aq_graph`), and container status.
- **Follow-ups filed:** `quick-impact-98`, `bold-delta-91` and `calm-beacon-15`
  (aq-platform, project root, so they do not block this gate). There are two
  aq-worker `CROSS-REPO FOLLOW-UP` comments on this task.
- **Limits:**
  - 1 of the 3 required settled runs, accepted by the owner.
  - The run-12 grant ID is recorded only as a prefix.
  - The checkpoint bytes were not re-read by MS0.
  - The IDs of the consolidated server I0 findings task were not recorded.
  - The demo is documented, not executed.
