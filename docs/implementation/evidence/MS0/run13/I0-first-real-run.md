# I0: first real ChatGPT native run qualification

Tasks `azure-horizon-22.1` (runs 9–11) and `calm-stone-85` (runs 12–13).

On 2026-11-03, **run 13 completed one real run end to end**, on aq-server
`2d337f36` (PR #50, keen-impact-58). A real Codex 0.166.0 session on the owner's
ChatGPT login made the requested one-line README edit in `ElectricJack/aq-scratch`.
The source was retained centrally. The owner's MS0 reviewer then settled it through
the public routes: combined prepare/admit, evaluation (503 until answered), combined
resolve (approve), evaluation (accepted) and completion reconcile (confirmed). The
login was not refreshed, so no writeback was needed.

Run 12, on aq-server `61b8d0e5`, could not be settled. That server had no settlement
or combined-review routes.

## Run 13 — 2026-11-03 (`calm-stone-85`)

Machine-readable, secret-free evidence:
[`evidence/i0-first-real-run/run13.json`](evidence/i0-first-real-run/run13.json).

Inputs:

- The qual server was rebuilt from aq-server main `2d337f36`, as image
  `sha256:f7f726c8…`. That revision mounts the settlement router, adds the
  production combined-decision path and accepts relay uploads without a watcher.
- Config changes, in [`definitions.py`](../../scripts/e0-qual/definitions.py) and
  [`pins.json`](../../scripts/e0-qual/pins.json):
  - The config now has a `[reviews.protection]` key.
  - `worker_i0.definition_version` is "3", `PROFILED_REVISION` 6 and
    `INTELLIGENCE_REVISION` 4. The new server pin changes the definition bodies.
- The worker image (`20abe635…`), node, profile and model are unchanged from run 12.
- New graph task `e0a10000-0000-4000-8000-000000000007` asks for a concrete edit.
  Append exactly `I0 run 13 (calm-stone-85): hello from aq-worker` to README.md,
  change nothing else, then submit the result.

| Invocation | Result | Cause |
| --- | --- | --- |
| run 13a | exit 2 in 2.8 s, "phase local observation expired"; Codex not launched, no lease | The accept lost the offer-window race by 22 ms. See below. |
| run 13b | exit 2 in 2.7 s, same error; Codex not launched, no lease | The proposal was sent 1 ms inside the local deadline, and its reply arrived after the deadline. |
| **run 13c** | **exit 0, 941.0 s**, "native run: AttemptStopped; retained state preserved" | See below. |

**Offer-window race (13a, 13b).**

- The server's offer deadline is the minimum of several bounds, including
  `selected.expires_at_ms` (aq-server `producer/build.rs:614`).
- `aq-selection` caps that selection TTL at `now + 2000 ms` (`select.rs:238`).
- The selection is computed about 0.7–0.9 s into the poll, so the offered window is
  about 1.4–1.8 s.
- The worker's local deadline starts at poll send (`http_profile.rs:88` `live()`),
  which counts the poll latency twice.
- With host load at 11–18 (one visible CPU, shared with other agents' builds), the
  accept misses that window. Run 12 won it by about 20 ms.
- For 13c, the qual containers and the worker got `--cpu-shares 8192`. They were
  reset to 1024 afterwards.
- 13c launched at load 11.23, its poll took 1.0 s and the accept succeeded.
- Neither 13a nor 13b leased the login or used Codex quota. Their state is
  preserved in the worker volume under `/state/preserved/run13a` and `run13b`.

**Run 13c.**

- Run IDs:
  - Container `aq-i0-run13`, 18:16:03Z to 18:31:46Z.
  - Attempt `f246a730-1fcd-5c62-a4a2-d98f4851eaab`, offer `dce3972e…`, generation
    3, grant `53437605…`.
- Every worker route in the run window answered 200. That includes all 423
  terminal-relay uploads; run 12 had 214 × 409 on the same uploads, so the
  upload-without-watcher fix works. There were 62 heartbeats and 32 artifact calls.
- Codex made the edit, checked it itself and **submitted its in-session result at
  18:16:38Z**, 35 s after launch.
- The session then stayed open until the 900 s wall limit, so report
  `095807a2-9dc3-4a16-be5a-9abbb26c2730` has process outcome **`expired`**. The
  in-session conclusion is recorded unchanged. The cause is in
  `runner/codex.rs`: the run loop ends only on cancel, deadline or natural pane exit,
  and a recorded result claim does not stop the session. That is a follow-up below.
- **Retained source change.**
  - Transfer `df5a8007…` is retained, with source receipt `74145d1f…`.
  - Checkpoint `17fe8a72` has the single parent `2e32645`.
  - The central object store was read directly. `README.md | 1 +` adds exactly the
    requested line, with a single trailing newline. Every earlier byte is unchanged.

**Settlement (owner MS0 reviewer, `:32848`, dev owner identity).**

| Step | Route | Result |
| --- | --- | --- |
| prepare | `POST /api/reviews/combined/prepare` | 200, subject `5530ae7f…` |
| admit | `POST /api/reviews/combined` | 200, decision `2c66a3c3…`, echoed subject equal |
| inspect | `GET /api/reviews/combined/{id}` | 200, pending, revision 1, criteria and recorded result shown |
| evaluate | `POST /api/settlement/evaluations` | 503 `updates_unavailable`, "combined review has no recorded answer" |
| resolve | `POST /api/reviews/combined/{id}/resolve` | 200, **approve**, revision 2, `human_answer` |
| evaluate (same intent) | `POST /api/settlement/evaluations` | 200, `d58ab4f8…` **accepted**, release `c821499f…` |
| lookup | `GET /api/settlement/evaluations/{op}` | 200, same receipt |
| reconcile | `POST /api/settlement/completions/b196d43a…/reconcile` | 200, **confirmed**, dispatch 1; graph task at version 2 / content revision 2; recovery none |

After settlement, the DB shows:

- `execution_attempts.holds_task` is false.
- The evaluation is accepted.
- `profiled_verified_releases` has `c821499f`, under the e0-qual terminal-refresh
  qualification.
- The completion is confirmed, with `outbox_acknowledged` still false.
- The source transfer is still retained.
- The attempt's state JSON (record revision 2) still says `offered` / retention
  `pending`. It was not updated by settlement.

**Login.** It was not refreshed, so no writeback was needed.

- After the run, these three are each byte-identical to the pre-run baseline (sha256
  prefix `98a9a084`, 4017 bytes):
  - the owner's `~/.codex/auth.json` (still 0600, with an unchanged mtime);
  - the leased material in the worker volume;
  - the server vault copy.
- There are no other login files in the worker volume, and `docker diff aq-i0-run13`
  shows no auth paths.
- The writeback path (`scripts/i0-auth-writeback.py`) therefore stays verified only
  with synthetic logins.
- `aq-i0-run13`, `aq-i0-run13a` and `aq-i0-run13b` are kept.

**Follow-ups found by run 13. Not filed: this token cannot list or create tasks.**

1. Worker: end the native session, or report a completed process outcome, once the
   in-session result is recorded. Today a completed edit holds Codex for the full
   wall limit and reports `expired`.
2. Server/worker: the profiled offer window, about 1.4–1.8 s from the 2 s selection
   TTL, is shorter than poll plus accept on a loaded host. The worker also counts
   poll latency twice.
3. Server: the attempt's state JSON and `outbox_acknowledged` are not updated after
   confirmed completion. This is cosmetic for settlement, but it misleads anyone
   inspecting the attempt.

## Run 12 — 2026-11-03 (`calm-stone-85`)

Machine-readable, secret-free evidence:
[`evidence/i0-first-real-run/run12.json`](evidence/i0-first-real-run/run12.json).

Inputs:

- Node `f761410b-43f5-4106-ab80-f044dd446da0`, profile `owner-chatgpt-codex`,
  `gpt-6.1-sol` high, target `ElectricJack/aq-scratch` `main`.
- Server `ElectricJack/aq-server` `61b8d0e5`, image `sha256:098efcb5…`.
- Worker image `sha256:20abe635…`, built from `f4703fe`. That revision contains the
  fixes for 22.1.1 (Codex companion files), 22.1.2 (workspace pre-trust), 22.1.3
  (stopped-attempt reconcile) and 22.1.4 (redacted failure causes).
- New seeded graph task `e0a10000-0000-4000-8000-000000000006`. The tasks for runs 9
  and 11 are still held.

| Invocation | Result | Cause |
| --- | --- | --- |
| run 12a | exit 2 in 0.7 s; no server contact, no material lease | The status tap ran outside the Docker host network, so the first enrollment heartbeat got "connection refused". The tap now runs as container `aq-i0-tap12`. |
| run 12-409 | exit 2 in 2.5 s; probe 200, profiled poll 409; not admitted, no lease | The server trigger `admission_definition_identity_guard` refuses a changed definition body at an unchanged version, with an opaque 409. Fixed by `worker_i0.definition_version` "2", `PROFILED_REVISION` 5 and `INTELLIGENCE_REVISION` 3 ([`definitions.py`](../../scripts/e0-qual/definitions.py)). The credential stays at version 1, bound to its registered image. |
| **run 12** | **exit 0, 916.4 s**, "native run: AttemptStopped; retained state preserved" | See below. |

**Run 12.**

- Run IDs:
  - Container `aq-i0-run12`, 15:09:48Z to 15:24:50Z.
  - Attempt `ab352201-950e-5a77-9008-a374c85f2f00`, grant `53c38069…`, launch
    `c4c9673b…`, terminal `c3c32118-c3f3-4b54-9066-15d0043bfb40`.
- Every worker route answered 200: probe, register, inventory, poll, proposal,
  accept, prepare, `started`, `stopped`, material lease, results and profiled
  source. The execution-source begin/manifest/bundle/finalize calls did too. There
  were 64 heartbeats and 32 artifact calls.
- No owner terminal input was sent. Unlike run 11, the report shows Codex
  inspected the task and attempted a result submit, so tool calls ran.
- Report `7439f3bc-d47d-4a7d-8f30-7b86f262d343` has outcome **`expired`**. Its
  conclusion says the pinned task named no concrete edit or acceptance criteria,
  so nothing was changed. The in-session result writer was unavailable and the
  initial submit was rejected; Codex did not claim completion.
- **First I0 retained source.** Retention handle `1640c116…` is retained and
  active, accepted 15:24:51Z. Its checkpoint `0b52d2d6…` equals base `2e32645`,
  so there is no source change.
- The server shows the attempt stopped (reason "reconciliation"), with `holds_task`
  true and retention `pending`.

**Terminal relay.**

- Every output upload was refused: 214 × 409 "watch snapshot resynchronization
  required" and 4 × 500.
- Cause, in aq-server `61b8d0e5`:
  - `Runtime::frame`/`snapshot` (`terminal/runtime.rs`) refuse when no buffer
    exists.
  - A buffer is only reserved by a Watch or Capture mutation (`terminal/service.rs`).
  - No owner watcher was attached, so every upload was refused.
- The run itself was unaffected.
- Relay `poll` and `output-poll` also returned 8 and 13 opaque 500s.

**Settlement.** These public routes were probed at `:32848` with the dev owner
identity, in protocol order:

- GET `/api/reviews/combined/{id}`
- GET `/api/reviews/decisions`
- POST `/api/settlement/evaluations`
- GET `/api/settlement/evaluations/{id}`
- POST `/api/settlement/completions/{id}/reconcile`

Each one answered 404 "no such route". Resolve (`POST /api/reviews/combined/{id}/resolve`)
needs a decision to exist first, so it was not called. Afterwards there are no
settlement evaluation, decision-request or completion rows.

Causes:

1. The reviews/settlement router is not mounted on the qualification server
   (`quick-glacier-78`: no human-review runtime, keyring or trusted Unix PG).
2. Even when mounted, evaluation requires an existing combined decision
   (`combined_completion_in_tx`). Combined decisions are created only by
   `prepare_combined_as` + `admit_combined` (`reviews/combined.rs`). Those have no
   HTTP or production caller, and `prepare_combined` also requires a Settled
   effects ledger.

So run 12 cannot be settled through the public routes on this server.

**Login.** None was refreshed.

- The login reached the container only through the private material lease. The
  leased material is byte-identical to the pre-run baseline.
- After the run, the owner's `~/.codex/auth.json` (0600, 4017 bytes) is still
  byte-identical to the baseline.
- No other login file exists in the worker state volume. `docker diff aq-i0-run12`
  shows no auth paths.
- No writeback was needed.
- `aq-i0-run12`, `aq-i0-run12-409` and `aq-i0-run12a` are kept.

**Remaining acceptance.**

1. Mount the settlement and combined-review routes on the qualification server.
2. Add a public way to admit a combined decision for a profiled report.
3. Give the run a task with a concrete edit, so the outcome can be
   `SourceChangeReviewed` instead of `expired`.
4. Attach a terminal watcher, or have the server accept uploads before a watch.

The node measurement expires 2026-11-04 07:22:45 UTC.

## aq-e0-qual runs 9–11 — 2026-11-03 (`azure-horizon-22.1`)

The reopen asked for one real run against `aq-e0-qual` with profile
`owner-chatgpt-codex`, target `ElectricJack/aq-scratch` `main`, on node
`f761410b-43f5-4106-ab80-f044dd446da0` (measured `gpt-6.1-sol` high). Each run
used a fresh single-slot state root in the `aq-e0-qual-worker-state` volume. The
container entry is [`scripts/e0-qual/i0-run-native.py`](../../scripts/e0-qual/i0-run-native.py):
it starts worker-side `aq-git`, registers the worker-mode binding for repository
`e0a10000-0000-4000-8000-000000000003`, then runs `aq-worker run-native` once.
[`scripts/e0-qual/status-tap.py`](../../scripts/e0-qual/status-tap.py) logged
request lines, statuses and public error codes only (no headers or bodies).

| Run | Attempt / grant | Codex / model | Wall time | Retained source | Settlement | Refresh |
| --- | --- | --- | --- | --- | --- | --- |
| 9 | `2d41bbec…` / granted | 0.166.0 / not reached | stopped after `started` | None | None | No |
| 10 | Not admitted | — | 0.3 s (exit 2) | None | None | No launch |
| 11 | `87d16454-c2cc-593c-9bfd-60ac4ea56911` / `d56e197c-3955-4dd0-bc52-2016724b7e58` | 0.166.0 / `gpt-6.1-sol` high | 944.4 s (exit 2) | None | Not reached | No |

**Run 9.** Graph task `e0a10000-0000-4000-8000-000000000004`. The qualification
server had no `[terminals]` configuration, so terminal-relay registration answered
503 `identity_unavailable` and the attempt stopped right after `started`. The
attempt still holds its graph task (`holds_task`), so the task cannot be offered
again. State is preserved at `/state/preserved/run9`.

**Run 10.** The executor `host_root` (`/state/host`) did not exist; run-native
exited 2 in 0.3 s with only its generic message. No admission occurred. A second
seeded graph task `e0a10000-0000-4000-8000-000000000005` was created with the
existing E0 seeding path, because run 9's task is held.

**Run 11.** Graph task `…0005`, terminal `dad76365-422a-4fa5-8074-2a6ff110692e`,
container `aq-i0-run11` from 09:31:21Z to 09:47:07Z. Every worker route answered
200 once: probe, register, inventory, poll, proposal, accept, prepare, `started`,
`stopped`, source/manifest/bundle and the private material lease
(`/v1/worker/profiled/material`). There were 62 heartbeats and 32 artifact calls.
Codex authenticated with the ChatGPT login (`auth_mode` ChatGPT, no API key; every
provider request was 200). The TUI stopped at **"Trust this folder?"**; the owner
stand-in selected "Trust and continue" by one terminal input through the public
relay (`POST /api/v1/terminals/{id}/mutations`). Codex then ran on `gpt-6.1-sol`
high, with one auxiliary turn on `gpt-5.6-luna`. Both tool calls failed before
execution because `/opt/aq-runtime/codex-code-mode-host` is missing from the image.
Codex reported "No files changed … No AQ result was submitted". The 15-minute
execution permit then expired. The host recorded phase `stopped`, reason
"execution permit expired", root status 0, 3 processes reaped. The server shows the
attempt finished/stopped with retention `pending`. It has no profiled result head,
report or settlement evaluation. The evaluation, combined-review resolve and
completion reconcile routes were therefore not called. State is preserved at
`/state/preserved/run11`.

**Login refresh.** None. After run 11 the owner's `~/.codex/auth.json` is
byte-identical to the pre-run baseline, mode 0600. No `auth.json` remained in the
launch's codex home, and no request carried an auth-recovery mode. The only
"refresh" log lines were `list_models refresh_strategy=online`. The login reached
the container only through the material lease from the server's 0600 vault copy.

**Qualification-stack fixes on this branch**
([`scripts/e0-qual/definitions.py`](../../scripts/e0-qual/definitions.py)):
`[terminals.protection]` with a generated 0600 relay key (run 9);
`SOURCE_DISK_BYTES` set to aq-git's real native reservation; `PROFILED_REVISION`
bumped to 2 because the server refuses changed profiled content at an unchanged
revision. The run entry creates the worker binding and needs `/state/host` to exist.

**Deviations.** `seccomp=unconfined` on the local qualification container
(owner-approved for I0/E0 only). The image is the prepared `dc62cfc` image
`sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d`, not a
rebuild from current main. A second seeded graph task was created. Owner stand-in
terminal input answered the trust prompt.

**Findings filed (direct):**

| Task | Finding |
| --- | --- |
| `azure-horizon-22.1.1` | The native image copies only `bin/codex`. Codex 0.166.0 also needs `codex-code-mode-host` (sha256 `657f4e90…d630`), `codex-path/rg` and `codex-resources/`. Every tool call fails. **I0 blocker.** |
| `azure-horizon-22.1.2` | The interactive executor does not pre-trust the workspace; the TUI waits on "Trust this folder?". |
| `azure-horizon-22.1.3` | run-native has no recovery path for a stopped, unsettled attempt; each failed run consumes a graph task. |
| `azure-horizon-22.1.4` | run-native/check-native hide the failure cause behind a generic message. |

A task comment starting `CROSS-REPO FOLLOW-UP (aq-server):` records the relay
"one terminal poll per node" 409 storm (219 in run 11) and one output-poll 500,
the generic 422 for over-2 s input deadlines, the missing server release for held
graph tasks, and `[terminals]` validation.

**Remaining acceptance.** Rebuild the native image with the full Codex package
layout (22.1.1) and workspace pre-trust (22.1.2). Release or replace the held graph
tasks (22.1.3 plus the aq-server follow-up). Then run once more and settle through
`POST /api/settlement/evaluations`, `POST /api/reviews/combined/{decision_id}/resolve`
and `POST /api/settlement/completions/{operation_id}/reconcile`.
The owner's node measurement expires 2026-11-04 07:22:45 UTC.

## Resume preflight — 2026-11-02 17:10 PST

The owner's reopen decision authorizes the existing ChatGPT login, atomic 0600
refresh writeback, and acting as the owner's MS0 settlement reviewer. It also
explicitly substitutes `origin/main` for the nonexistent `origin/dev`. Those
decisions are settled; no further login or review permission is requested.

Claim epoch 3 resumed on branch head
`8301a653146d41aad2d9f49c39fb3d76b451c730`, which already includes current
`origin/main` `aa6c80728413da18eaf49fc7ca3472afe8ccee78`. The historical image
below still contains worker `dc62cfc`; it must be rebuilt from the current main
before any real attempt. Earlier physical checks do not qualify changed code.

[Fresh preflight evidence](evidence/i0-first-real-run/resume-preflight.json)
records the current retained E0 setup. All four `aq-e0-final` services are
healthy. Its vault still omits `intelligence`, `profiled_execution`, `playbooks`
and `terminals`. The active catalog remains revision 1 with the same digest.
An evaluation POST, completion-reconcile POST and combined-review GET all return
HTTP 404 with `no such route`. These bounded requests created no attempt or
settlement decision. No real credential was read, copied or delivered.

Execution still needs the qualified runtime/harness/environment/context inputs
and the enabled human-settlement runtime. The server context owner explicitly
requires qualified counting/capacity evidence for the exact selected model,
credential revision and adapter; hashing fixture declarations does not produce
that evidence. The settlement owner requires protected configuration and a
trusted Unix PostgreSQL endpoint, absent from the E0 handoff. These remain the
previously filed findings `sharp-harbor-18` and `quick-glacier-78`; no duplicate
task was filed. The supervisor was asked for the qualified input location and settlement-runtime
handoff in message `msg-61e8d6158d584c2eae02c0ca9768c300`. No reply was pending
when the user subsequently requested closing or continuing work after ten minutes.
Close as failed/blocked with the published evidence; acceptance remains **0/3**
and there has been no login refresh.

The following sections retain the previous session's preparation and checks;
their missing-owner-authorization and absent-main-substitution statements are
historical and are superseded by the decision above.

## Prepared and verified

The source baseline and image worker source are current-main revision
`dc62cfc0faf2109485b1c9f17a024f6fcb37ebd5`. The plan was committed first as
`13fadfe`; initial helper/tests were committed as `3ba25b4`, followed by the
hardening recorded below. Final publication identifies the evidence branch revision.
Only aq-worker files were edited; aq-server's worker candidate was not repinned.

The isolated build copied `Dockerfile.worker` and its pinned Git inputs from E0's
archived aq-server source `8f6c49fd91850b924524494e26c5c17d14072796`, replaced only
the isolated worker source archive with `git archive origin/main`, and added the
native helper/result client, installed Codex ELF, ripgrep, tmux 3.4 and runtime
libraries. No operator home or credential entered the build context.

Final local image:
`sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d`.
It reports worker 0.5.1, Codex CLI 0.166.0, tmux 3.4 and Git 2.56.0.
[Build inputs](evidence/i0-first-real-run/build-inputs.json),
[Dockerfile](evidence/i0-first-real-run/Dockerfile.worker),
[image identity](evidence/i0-first-real-run/image.json) and
[installed file pins](evidence/i0-first-real-run/installation.json) are retained.
The executed final build command, using the retained credential-free context, was:

```sh
docker build --progress=plain --target i0 \
  --build-arg AQ_SOURCE_REVISION=dc62cfc0faf2109485b1c9f17a024f6fcb37ebd5 \
  --build-arg AQ_WORKER_ARCHIVE_SHA256=38dfa8fed216d7bc2621d68d58550849714fdd009db4006fadfffb743356ef01 \
  -t aq-i0-worker:dc62cfc /home/jkern/.local/state/aq-platform-i0/build
```

The image is local and was not published to a registry. Debian package repositories
and the original unqualified Rust build-stage tag prevent a bit-for-bit rebuild
claim; the observed final image ID is immutable.

Environment: WSL2 Linux `6.18.33.2-microsoft-standard-WSL2`, x86_64;
Docker client/server 29.3.1, Compose v5.1.1; Rust 1.96.1; Python 3.12.3.
Build jobs were capped at two for the worker and tmux extension. No test-support
feature, fake provider or provider override was built into the worker image.

The existing bootstrap's `inspect` mode ran in the final image with host networking,
UID/GID 10001, read-only root, all capability sets empty and no-new-privileges.
It verified single-link program files and a separate persistent state filesystem.
[Physical observation](evidence/i0-first-real-run/physical-preflight.json) is metadata,
not an independent runtime/model qualification or a `check-native` success.
The stopped credential-free inspector containers and their empty state are retained.

A credential-free `codex sandbox -- /bin/true` probe with private writable
`CODEX_HOME` under `/state`, a writable `/tmp`, read-only root, dropped capabilities
and no-new-privileges failed under Docker's default seccomp policy. Adding
`--security-opt seccomp=unconfined` returned exit 0 with the other constraints intact.
[Exact corrected requests/results](evidence/i0-first-real-run/sandbox-probes-corrected.json)
are retained. An earlier probe used obsolete `sandbox linux` syntax; the installed
CLI's help corrected it, and that invocation is not counted as a valid sandbox test.
Probes with a read-only default home or a home under `/tmp` also exposed alias setup
requirements; the successful probe uses a private home outside `/tmp`.
The mapper's interactive argument prefix parses successfully with `--help`
([result](evidence/i0-first-real-run/interactive-argv-help.json)); this is parsing,
not a model call. These probes make no hostile-code isolation claim.

## Blockers and owner decision

E0's final stack is retained at `/home/jkern/.local/state/aq-platform-e0-final`;
all four services were healthy. Admin, worker and Graph URLs are respectively
`http://127.0.0.1:32838`, `http://127.0.0.1:32839` and
`http://127.0.0.1:32837`. The designated scratch repository is
`https://github.com/ElectricJack/aq-scratch.git`, public branch `main`, credential
reference `none`. E0 retained base `2e32645bab743729bfaeb0f2eec417c2ace7794b`
under handle `e0000000-0000-4000-8000-000000000011`. That unchanged E0 base is
not I0 output.

The active profile catalog is revision 1, digest
`0c8a041de2e1af9e6937cd0c87cefed8a0c82d6e592d986c7d0ab3dfc817d6f2`.
Its provider/model are `configuration-required` and credential references are empty.
E0 vault configuration has no intelligence, profiled_execution, playbooks or
terminals owner configuration. Current native admission requires exact qualified
runtime/harness/environment/scoped-presence inputs and matching model/credential
context counting and capacity evidence. The checked-in physical fixture evidence
explicitly excludes production model/environment qualification. Arbitrary fixture
hashes were not installed as real qualification. Production `check-native` and
`run-native` were therefore not attempted with invented configuration.

The completion reconcile endpoint returns HTTP 404 `no such route` for a nonnil
operation ID and `{}`. Source inspection confirms the routes are installed with an
enabled human-review playbook runtime; E0 omits it. That owner also requires a
protected engine keyring/connection and trusted Unix PostgreSQL socket, absent
from this handoff. [Owner route observations](evidence/i0-first-real-run/owner-route-preflight.json)
include a `/dependency-readyz` request sent to the admin listener; its 404 is not
counted as a readiness defect because that probe used the wrong listener.

Supervisor requests `msg-a3c7cdd4da6d48c798c4dd8556d42d4b` and
`msg-0780e4861b3d42b996ce272471e25784` request upstream merge substitution and
qualified setup location/guidance. Owner dashboard message
`msg-ab792460ea124c03b8edf522083d613f` requests the missing qualified inputs or
an explicit bounded qualification decision, plus settlement runtime ownership.
No reply or waiver has been recorded at this report revision. The imported
`aq-platform` message direction was refused by the live `aq-worker` project scope;
all subsequent requests used the authorized project. Wait registration is disabled
for this project; no unsupported durable wait was registered.

## Login reconciliation helper and verification

`scripts/i0-auth-writeback.py` is manual qualification glue. It validates bounded
private regular login files, refuses symlinks and duplicate JSON fields, compares
an initial snapshot with the current owner file, writes exact refreshed bytes to a
0600 sibling, fsyncs, atomically renames and fsyncs the directory. It prints status
only. Its schema matches native login validation, including required bounded account ID,
ASCII token bounds and rejection of unknown fields. Changed owner bytes cause refusal with both inputs preserved. Missing launch
output and unchanged login preserve the owner file. The helper has only been used
with synthetic credentials. It does not coordinate arbitrary concurrent writers:
there is a compare/rename race, so owner writers must be quiescent during manual
reconciliation. It does not implement a vault or update the server material registry.
No container with real credentials may be removed until source and login state are
reconciled; retain it and its private state on any conflict or uncertain failure.

An actual final-image setup failure and helper owner-conflict refusal used synthetic
login files only. Both returned exit 2, all private files remained byte-identical
at mode 0600, and Docker inspection confirmed the exited container was retained
with auto-remove disabled. [Request and observations](evidence/i0-first-real-run/failure-preservation.json)
are retained. This verifies setup/refusal preservation before admission; it does
not verify post-launch real credential or source reconciliation.

Commands and observed results:

| Verification | Result |
| --- | --- |
| `aq test tools/qualification/tests/test_i0_auth_writeback.py -q` before helper | RED: 6 failed, helper absent |
| Same command after initial helper | GREEN: 6 passed |
| Added aged-atime and duplicate-field cases | RED: 2 failed, 6 passed |
| Same command after hardening | GREEN: 8 passed |
| Added native-invalid login cases after fresh review | RED: 10 failed, 8 passed |
| Same command after native-schema validation fix | GREEN: 18 passed |
| Actual synthetic setup/conflict failure in final image | PASS: exit 2, private files unchanged, container retained |
| `python3 -m py_compile scripts/i0-auth-writeback.py tools/qualification/tests/test_i0_auth_writeback.py` | PASS |
| `git diff --check` | PASS |
| Docker build, `CARGO_BUILD_JOBS=2 cargo build --locked --release --bins` | PASS, final image above |
| Final-image worker/Codex/tmux/Git version checks | PASS |
| Existing bootstrap `inspect` on final image | PASS, metadata only |
| Correct sandbox probe, default seccomp | FAIL, bubblewrap namespace refusal |
| Correct sandbox probe, seccomp unconfined | PASS, no provider call |
| Mapper argument prefix with `--help` | PASS, no provider call |
| `cargo fmt --all -- --check` in nested worktree | FAIL, vendored crate resolves outer workspace |
| Same fmt command from standalone current-main archive | FAIL, existing import order in `native/result/source.rs` |

AQ's Python wrapper selected its configured fallback interpreter and used its normal
six-worker/default marker configuration. Test logs are `/tmp/i0-auth-red.log`,
`/tmp/i0-auth-green.log`, `/tmp/i0-auth-hardening-red.log` and
`/tmp/i0-auth-hardening-green.log`, `/tmp/i0-auth-review-red.log` and
`/tmp/i0-auth-review-green.log`. Build logs are `/tmp/i0-image-build.log`,
`/tmp/i0-image-tmux-build.log`, `/tmp/i0-image-final-build.log`; fmt logs are
`/tmp/i0-fmt.log` and `/tmp/i0-fmt-standalone.log`. Persistent build inputs and
observations are under `/home/jkern/.local/state/aq-platform-i0`.
No Rust source changed; additional full workspace Clippy/test suites were not run.
The required real-provider acceptance remains unverified.

## Filed findings and remaining acceptance

| v1 task | Confirmed finding |
| --- | --- |
| `sharp-harbor-18` | Missing real native qualification/configuration inputs |
| `quick-glacier-78` | Human settlement runtime/routes absent from local stack |
| `fresh-delta-13` | Changed ChatGPT login is preserved by cleanup refusal; durable owner/server reconciliation missing |
| `vivid-bridge-32` | Imported project/supervisor directions exceed live project scope |
| `amber-horizon-91` | Native image needs current worker, helper/client, pinned tmux/Codex and full shared-library dependencies |
| `stark-grove-17` | Baseline formatting and nested-worktree metadata failures |
| `sharp-dune-88` | Default Docker seccomp blocks the actual credential-free Codex sandbox probe |

The helper defects found during TDD and fresh review were fixed and are covered
by the final 18-case suite; durable reconciliation remains tracked separately.

A fresh whole-branch reviewer found two important gaps: native-schema parity and
missing actual early-failure preservation evidence. The single fix pass reproduced
ten invalid-login acceptances before tightening validation, then passed all 18
tests. The synthetic Docker failure probe above addresses the bounded setup case.
There were no critical or minor findings. The review supports publishing partial
preparation only; all real-run acceptance remains unmet.

Decisions retained from the execution ledger, in order:

1. Do not promote fixture hashes to genuine qualification; owner inputs or an
   explicit bounded qualification method are needed. Cost if wrong: delayed runs.
2. Build pinned tmux 3.4 because the native host requires it. Cost if wrong:
   unnecessary image-build work.
3. Report unmet acceptance without inferring end-to-end success from unit tests.
   Cost if wrong: an overly conservative task result.
4. Close fail/blocked when owner inputs remain unavailable, as the task requires.
   Cost if wrong: the owner must resume the task.
5. The reviewer's real provider/source/settlement/rotation exclusions remain unmet
   acceptance, so this evidence cannot pass I0. Cost if wrong: delayed qualification.
6. Manual writeback requires quiescent owner writers; durable multiwriter support
   is follow-up `fresh-delta-13`. Cost if that precondition is violated: lost rotation.
7. Exact observed image identity is sufficient for these probes; floating build
   dependencies prevent a reproducibility claim. Cost if wrong: rebuild divergence.
8. Root inspection and AQ receipts support external E0/configuration/task claims
   the reviewer did not re-query. Cost if wrong: stale external evidence.
9. Plain Docker probes make no hostile-code isolation claim. Cost if treated as
   such a boundary: insufficient isolation.
10. The assigned publication/close instructions govern handoff; no whole CI run,
    shared-branch merge or extra approval menu is needed. The absent `origin/dev`
    and unanswered substitution request leave the upstream merge unmet. Cost if
    wrong: delayed integration or broader checks needed later.

| Run | Attempt/grant | Codex/model | Wall time | Retained source | Settlement | Refresh |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | Not admitted | 0.166.0 / not selected | Not started | None | None | No launch |
| 2 | Not admitted | 0.166.0 / not selected | Not started | None | None | No launch |
| 3 | Not admitted | 0.166.0 / not selected | Not started | None | None | No launch |

To resume, supply the missing qualified configuration and enabled settlement owner,
then run production `check-native` against actual installed pins before delivering
login through the existing material lease. Each real invocation needs its own fresh
empty single-slot state root. Preserve original and refreshed private login files,
reconcile owner state before disposal, retain edits centrally, obtain actual human
review input, and reconcile each completion through the specified public routes.
The task must close fail unless all three consecutive runs are evidenced.
