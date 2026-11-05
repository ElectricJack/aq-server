# I0: first real ChatGPT native run qualification

Task `azure-horizon-22.1`. **Acceptance is unmet.** On 2026-11-03, run 11 reached
real Codex 0.166.0 on the owner's ChatGPT login through `aq-worker run-native`
against `aq-e0-qual`, but no source was retained and settlement was not reached.
The owner's `~/.codex/auth.json` was not refreshed and still equals its baseline.

## aq-e0-qual runs — 2026-11-03

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
