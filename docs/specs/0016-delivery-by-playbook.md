# 0016: Delivery by playbook (Epic 0: AQ delivers its own work)

Date: 2026-11-17. Contract for aq-server, aq-git, aq-worker, aq-playbooks and
aq-dashboard. Status: draft for owner review.

## 1. Intent

Owner: "If we want results to leave AQ we need that work defined by playbook
policy, similar to how it was in v1. I won't accept policy getting hard-coded
here." Also, from 2026-11-15: "we always merge to main after checks pass, and we
should update from main at the start of every subsequent task. Once a PR is
merged to main we should delete the branch for the ticket." And from 2026-11-17:
"No it should just push to the repo."

The rule this spec applies everywhere: **code provides operations, facts and
events; a playbook decides.** Nothing in code chooses whether a result is
accepted, sent back, published, merged or escalated.

Owner decisions of 2026-11-17:

- Every gate moves to the playbook: acceptance, required checks and delivery.
- The worker node pushes its result to the central aq-git. A playbook operation
  then publishes the branch to GitHub. Nothing pushes to GitHub except central
  aq-git.
- GitHub is reached as a GitHub App (installation tokens), as in v1.
- Epic 0 is the first work AQ runs on itself. Until §9 step 9 is live, Claude
  carries each result out, opens the PR and merges it by hand.

## 2. What exists and what is wrong with it (origin/main a5a07443)

| Today | Where | Why it goes |
|---|---|---|
| A project is pinned to one commit and re-pinned by hand after each merge | vault `[profiled_execution] projects`, `scripts/ms1-repin-project.py` | Tasks must start from current main. A named ref exists (`SourceSelection::NamedRef`), but it follows only AQ's own publications (`profiled_execution/source.rs:179-206`) and never fetches from GitHub. `repo.sync` is called only at onboarding. |
| Each result is uploaded as a git bundle through `/v1/execution/sources` and imported by aq-git | `aq-worker/src/source.rs`, `transfer_client.rs`, `aq-server/src/source_retention.rs:171-188` | The owner wants plain push and fetch. Starting a task downloads a whole-history bundle (`source_retention/profiled_base.rs:250`). |
| The server decides a result's fate in a fixed order: acceptance, retained source, required checks, delivery | `settlement/default_policy.rs:78-112`, `settlement/automatic.rs` | This is policy in code. |
| `delivery: required \| not_required`, a policy body in server TOML | `definition_bodies`, `default_policy::parse` | This is policy in configuration. |
| Automatic publish to one branch, fast-forward only | `settlement/publication.rs`, `PublicationPolicy` in the binding, onboarding `allowed_targets:[default]` (`repository_onboarding.rs:554-556`) | This is policy in code, and it blocks per-task branches. |
| The GitHub side is a stub that can only open a PR and read checks, and PR mode is refused before it is called | `aq-delivery/src/policy.rs:134-195`, `NoGitHub` in `delivery_port.rs:21` and `publication.rs:114` | Nothing can open, merge or delete. |
| A held settlement is never re-evaluated | `settle_after_record` is called only from a result POST (`results.rs:80`) | Disappears with §6: the decision is level-triggered. |
| The playbook host sends one event on its own (`aq.work.route_needed`). `task.completed` exists only on the human path and on other profiles | `playbooks/triggers.rs`, `settlement_events.rs:10-15` | A delivery playbook has nothing to start from. |
| Playbooks cannot send a task back, open or merge a PR, or message a supervisor | catalog 2.2.0, `messages.rs:59-63` (owner-only) | These operations are missing. |

aq-git already has what per-task branches need: `allowed_target_prefixes` (for
example `refs/heads/aq/`) may be created and replaced under a lease
(`aq-git-core/src/repository/mod.rs:60-99`). Onboarding never sets it.

## 3. Branches and the life of a result

- `main` is the project's default branch on GitHub. Central aq-git keeps a
  retained copy of it, synced as described in §4.
- Each task has one branch, `refs/heads/aq/<task-id>`, in central aq-git. If
  the policy publishes it, it goes to GitHub under the same name. Every attempt
  of the same task, including repairs, pushes to that branch, so its PR stays
  the same.
- A task's first attempt starts from the latest retained `main`. An attempt
  after `work.return` (§6.2) starts from the task branch. With
  `update_from_main: true` it also has current `main` merged in; the node
  commits the merge, and conflicts are left as conflict markers that the return
  detail lists.
- The agent sandbox still has no git remote and no credential. The node, which
  runs outside the sandbox, is the one that fetches and pushes. SBX17
  (`hydrate.rs:3-6`) is restated as: *the sandbox never contacts a remote, and
  the node contacts only central aq-git, with a credential scoped to one
  attempt.*

## 4. Following main (E0.1)

- Central aq-git syncs each bound project's default branch from GitHub
  (`repo.sync`, already central-only). It does so:
  1. at an interval, set in **Settings › Delivery › Main sync interval**
     (default 60 s);
  2. when an offer finds the observation older than **Settings › Delivery ›
     Main freshness** (default 30 s; this is the constant at
     `source.rs:338-358` today);
  3. when a playbook operation asks for it (`main.sync`, §6.2).
- Every synced head is retained under a handle before it can be offered. This is
  a new aq-git operation, `repo.retain_ref` (sync, then pin the head, then
  return the handle). It replaces the `workspace.create` / `checkpoint` /
  `retire` sequence in the repin script.
- The project's source selection becomes the named ref `refs/heads/<default>`.
  Policy validation accepts a handle that `repo.retain_ref` returned for that
  ref, so moving `main` needs no config revision and no restart.
- The recorded publication head no longer outranks what is observed on GitHub.
  `main` is whatever the last sync saw.
- Deleted: `scripts/ms1-repin-project.py`, `SourceSelection::Immutable` for
  bound projects, and the precedence branch at `source.rs:195-206`.

## 5. Push and fetch through central aq-git (E0.2–E0.4)

### 5.1 aq-git: a plain git server for the project repository (E0.2), revised 2026-11-18

Owner: "a central aq-git with a per-project repository - tasks will always be on branches. Worker node pulls
from git repo on server using some local credential for the central repo, and then when it's done it pushes
back to the server repo, on the server aq-git then pushes to origin."

- **The repository:** central aq-git hosts one ordinary bare repository per project. It holds a real
  `refs/heads/main`, kept current from origin by `repo.sync` and `repo.retain_ref`, and the task branches
  `refs/heads/aq/<task-id>`.
- **Access:** a worker node fetches and pushes over git smart-HTTP through aq-server. The node uses its own
  credential, one per node for all projects, and aq-server adds the attempt's grant: the task ref, the base
  and the size limit.
- **Pushes use git's own safety:** quarantine, `receive.fsckObjects`, `receive.maxInputSize`. A minimal
  check allows updates only to the grant's `refs/heads/aq/<task-id>`: never `main`, nothing outside `aq/`,
  no deletes. A lease-checked non-fast-forward is allowed, for a returned task that merged `main`.
- **The branch ref is the record of a result.** A result report names the ref and the commit, and aq-server
  confirms the ref is at that commit. There is no separate push receipt or retention handle: the ref keeps
  its commits.
- **aq-git pushes task branches to origin** (`branch.publish`), deletes them after merge (`ref.delete`),
  and runs `git gc`.
- **What happens to branches is policy,** decided by the delivery playbook and a server-side agent, not by
  code: PRs, renames, dev, staging and release branches, cleanup of abandoned branches.

The first design (G2, behind the flag) kept AQ receipts and retention for pushes alongside git's own refs.
It failed two adversarial reviews because the two could disagree. Its journal, receipts and rollback are
removed (G2d).

### 5.2 Server (E0.3), revised 2026-11-19

- **The grant.** Each offer made while the server serves git has a grant,
  recorded with the attempt (`delivery_git_grants`). The frozen offer types
  (aq-admission `startup.rs`, `control.rs`, whose source bytes are pinned)
  do not carry it: the node reads it with `GET /v1/git/grants/<attempt-id>`
  (node credential; 404 unless the attempt is that node's, holds its task and
  has recorded no result).
  It is `{endpoint, repository_id, base, task_ref, lease, update_from_main}`:
  `endpoint` is `/v1/git/<repository_id>` on the worker origin; `base` is the
  commit this offer's source resolved to: for a first attempt the retained
  `main` head when it was offered, but for a replacement (a returned task) the
  replaced attempt's source, so not current `main`; `task_ref` is `refs/heads/aq/<task-id>`; `lease` is the task
  ref's head in central aq-git when the offer was made (`ref.show`), or null
  when the branch does not exist. A present lease means the task was returned
  or handed off: the attempt starts from the lease, and with
  `update_from_main: true` (the task's latest delivery decision returned it
  so) merges `refs/heads/main` as fetched from `endpoint` first (§3), never
  `base`. aq-git keeps that branch at the synced origin head. There is no
  token.
- **The route.** `GET <endpoint>/info/refs?service=git-upload-pack|git-receive-pack`,
  `POST <endpoint>/git-upload-pack`, `POST <endpoint>/git-receive-pack`. Every
  request sends `Authorization: Bearer <node credential>` (the node's own, one
  for all projects) and `AQ-Attempt: <attempt-id>`. The server serves it only
  while that attempt is the authenticated node's, holds its task, has a grant
  for this repository and has recorded no result. It forwards to aq-git's git
  socket with only the body headers of the request and its own grant headers
  (`AQ-Repository`, `AQ-Base`, `AQ-Task-Ref`, `AQ-Max-Push-Bytes`,
  `AQ-Operation`); client `AQ-*` headers are dropped.
- **The push check, in the server.** A receive-pack request may carry no
  command (git's probe) or exactly one: `<lease or 40 zeros> <new> <task_ref>`
  with `new` not zero. Any other ref, a deletion or a second command is 403; an
  old head other than the lease is 409; a body over Settings
  `delivery-limits.max_push_bytes` is 413 (or the transfer is cut off); an
  encoded body or a malformed command list is 400. History is not checked: a
  task branch that merged a newer `main` than `base` is accepted. aq-git then
  applies its own checks (§5.1).
- **Errors.** 400 no or malformed `AQ-Attempt`, bad request; 401 no or invalid
  node credential; 403 no live grant for this node, attempt and repository, or
  a push outside the grant; 404 not a git route, or no git endpoint
  configured; 409 stale lease; 413 too large; 503 database, Settings or aq-git
  unavailable, or the transfer passed Settings `git_transfer_timeout_ms`.
  Other statuses are aq-git's own. The node runs
  `git push --force-with-lease=<task_ref>:<lease>` (an empty lease when
  absent), so git itself reports a moved branch as `stale info` before
  sending; the node reports that as `push_conflict` and never forces.
- Transfer calls are not counted under the 30 s Bulk deadline
  (`admission.rs`); their limit is the Setting above.
- **The result report.** The frozen `ResultReport` is unchanged. A node that
  pushed sends `AQ-Pushed-Ref: refs/heads/aq/<task-id>` and
  `AQ-Pushed-Head: <40-hex commit>` on `POST /v1/worker/profiled/results`
  (both or neither); `source_receipt_ids` are no longer read. Before a new
  report is recorded the server checks that the ref is the attempt's granted
  task ref and that aq-git's `ref.show` has it at that head: 400 when the
  headers are malformed or the ref is outside `refs/heads/aq/`, 409 for
  another ref, a missing ref or another head, 503 when aq-git does not answer
  (retry the same report). A replay of a recorded report is not checked again.
  The confirmed head is stored with the report (`delivery_pushed_results`);
  "the result's source is retained" means the report named the task ref at
  its head, and no retention handle is taken. Once an attempt has recorded a
  result, `/v1/git` refuses it (403) and its grant is no longer served (404).
  An attempt that pushed nothing sends neither header.
- **Push conflict.** When git refuses the push as `stale info` (the task ref
  moved away from the grant's lease), the node does not force and does not
  hold the attempt: it sends its result request with `AQ-Push-Conflict: true`
  instead of the `AQ-Pushed-*` headers (the frozen report keeps the process
  outcome the agent had). The server records it only if aq-git's `ref.show`
  shows the task ref away from the lease (409 when it is still at the lease,
  503 when aq-git does not answer). The result then has no source, and
  `work.read` reports its `outcome` as `push_conflict`, so the delivery
  playbook can send the task back (`work.return`, usually with
  `update_from_main`).
- Onboarding registers `allowed_target_prefixes: ["refs/heads/aq/"]` beside the
  default branch. Existing bindings are migrated with
  `scripts/add-task-ref-prefix.py`.

### 5.3 Worker (E0.4)

- At start, the node clones from central aq-git (the main head, or the task
  branch, §3) into the slot's workspace. This replaces the bundle download and
  `artifact.import` in `hydrate.rs`.
- At finish, after the guardian's stopped-writer proof, the node commits the
  tree, as `workspace.checkpoint mode:all` does today, and pushes it to the task
  ref with the lease. The secret scan stays on the node, before the commit, as
  it is now (`sandbox_driver.rs:571-661`).
- A push that is refused or lost is retried with the same lease. If the lease
  no longer matches, the attempt is reported as `push_conflict`; the node never
  forces a push.
- Deleted when §9 step 11 lands: `transfer_client.rs`, the bundle path of
  `export_source`, and the source-transfer and source-download routes on the
  server.

## 6. Facts, events and operations for playbooks (E0.5–E0.6)

### 6.1 Event `aq.work.result_ready` (host-emitted, level-triggered)

The host emits it for a task when the task's current attempt has stopped and
recorded a result, and no playbook decision (§6.2 `work.complete`,
`work.return` or `work.hold`) has been made on that result. Emission rules copy
`route_needed` (`triggers.rs:196-284`):
- re-emitted every **Settings › Delivery › Decision repeat** (default 2 min);
- never while a run started for the same result is unfinished;
- also emitted when a fact the playbook can read changes (checks finish, `main`
  moves).

This is also what fixes the held-settlement fault: nothing is decided once and
then forgotten.

Payload: `{task_id, project_id, attempt_id, result_id, version}`. Everything
else is read through `work.read`.

### 6.2 Commands (host catalog 2.3.0)

| Command | Kind | What it does (mechanism only) |
|---|---|---|
| `work.read` (extended) | sync | Adds the result facts: `outcome` (agent's report), `changed` (bool), `branch`, `head`, `base`, `main_head`, `returns` (host-counted, per task), `pull_request` `{number, url, state, mergeable, checks}` if one is open. It answers for every bound project, not only routed ones. |
| `work.complete` | sync | Settles the task as completed through the existing `prepare_locked` / `dispatch` path. It fences on the current attempt, its report head and its stop proof, and releases the claim only on a validated Graph receipt. |
| `work.return` | sync | Puts the task back in the queue for a new attempt from its branch (§3). Parameters: `reason` (a code), `detail` (a fact value passed through, such as the failed check names), `update_from_main` (bool). Increments `returns`. |
| `work.hold` | sync | Records "waiting for the owner" with a reason code. The task stays held until a later decision. |
| `main.sync` | async | Syncs and retains `main` now (§4). |
| `branch.publish` | async | Central aq-git pushes the task branch to GitHub under the same name, with a lease. Completes with `published`, `conflict` or `refused`. |
| `pull_request.open` | async | Opens a PR for the task branch against `main`, or finds the one already open. Title and body come from the task and its report. |
| `pull_request.await_checks` | async | Completes exactly once: `passed`, `failed` (with the failing names), `conflict`, `closed` or `timeout`. "Required" means what GitHub's branch protection or rulesets require on `main`, unless the playbook passes a list of names. The deadline is a parameter, bounded by **Settings › Delivery › Longest check wait**. |
| `pull_request.merge` | async | Merges with a `method` parameter (`merge`, `squash` or `rebase`) and an expected head SHA. It deletes the branch on GitHub and in central aq-git. Completes with `merged`, `conflict`, `not_mergeable` or `refused`. |
| `pull_request.close` | async | Closes the PR and deletes the branch. |
| `supervisor.notify` | sync | Sends a message to the project's supervisor, with an `identity` so it is sent only once. The supervisor decides whether to escalate to Discord. |

Events added to the catalog: `aq.work.result_ready`, plus each async command's
completion, delivered through `aq.operation.updated`, as today. Every command
records its receipt in the run's timeline, so the Work page can show what the
playbook did with a result.

The catalog's `notes.json` documents each command and event. That is what the
authoring agent reads (`aq-playbook-authoring/src/request.rs:12-27`).

### 6.3 GitHub App adapter (E0.5)

- It lives in `aq-delivery`, replacing the `GitHubAdapter` trait's stub. The new
  trait has `open_or_find`, `checks`, `mergeability`, `merge`, `close` and
  `delete_branch`.
- Tokens are installation tokens for one repository, minted per call and cached
  until they expire.
- Configuration in the vault: App id, private key file, and the installation for
  each owner or organisation. No GitHub credential reaches a worker.
- Central aq-git's `branch.publish` uses the same installation token as an HTTPS
  credential profile, limited to `refs/heads/aq/`.
- Rate-limit and abuse responses come back as `retryable` with the reset time.
  The adapter does not choose to wait; the playbook decides.

## 7. Settlement records facts only (E0.7)

The result path (`results::record`) records the report and the push receipt,
gathers the same facts `evaluate_locked` gathers today, stores them as the
result's facts, and lets §6.1 emit. It returns no verdict.

Deleted, behind the epic flag until step 11:
- `settlement/default_policy.rs`, and `settlement.rs:24-30` with its caller in
  `producer/build.rs`;
- the verdict half of `settlement/automatic.rs`, and its `held` and
  `rework_required` dispositions;
- in `settlement/publication.rs`: the automatic publish (`deliver_then_settle`,
  `awaits_delivery`, `Flight`) and its `NoGitHub`;
- `PublicationPolicy` in the project binding, the `aq.default-completion.v1`
  body in `definition_bodies`, and `ProjectBinding.policy`'s completion role;
- `ReviewRequirement::PolicyActor` / `HumanOwner` selection at offer time. A
  policy that wants a person to approve uses `decision.request`, which already
  exists;
- `required_checks` in server TOML (`playbooks/config.rs:61-73`) and the
  `policy_unavailable` refusal that depends on it;
- the `delivery_unpublished` replacement block (`replacement.rs:120-142`) and
  the heuristics on the conclusion text (`claimed_changes_missing`,
  `agent_blocked`);
- `scripts/ms1-profiled.py`'s policy generator.

Kept as mechanism: the locks, idempotent dispatch before the Graph write,
receipt validation, the report-head, revoked and cancelled fences, claim
release only on a Graph receipt, and replacement facts. The release-proof
guards in migrations 0059, 0065, 0066 and 0133 accept the receipt from
`work.complete` in place of a confirmed automatic row. A migration drops
`profiled_auto_settlements`' policy columns.

A project with no active delivery playbook keeps its results waiting. The task
shows "result ready, no delivery policy". This is not a fallback policy: no
decision is made.

### Shared-host enrollment

`[delivery].project_allowlist` takes AQ project UUIDs and defaults to an
empty list. Optional `[delivery].task_allowlist` narrows enrollment to the
listed Graph task UUIDs within those projects. Omit it to enroll every task
in the listed projects. Set it to `[]` to enroll none. Enrollment requires
`git_http_socket` and source retention.

The server chooses the mode before observing an offer's source. It follows
the external head for an enrolled named-ref offer and uses the existing
source policy for other offers. Project-wide enrollment also follows named
refs in the background. An offer writes its Git grant in the same
transaction as the attempt. Attempts with a stored grant use Git push
evidence and delivery playbook settlement. Attempts without a grant
continue to use bundle evidence and the review or default policy pinned by
their offer. Keep the legacy source and validation policies available for
other tasks in the pilot project. New replacement offers use the current
allowlists. Keep a returned pilot enrolled while its successor needs the
saved task branch, and keep Git HTTP and the Host10 delivery binding
available while existing grants finish.

On the worker listener, the node credential can read its attempt's stored
grant at `GET /v1/git/grants/<attempt>`, including after a result or a
restart. A 404 means the attempt has no grant. A 401, 403 or 5xx is an error
for the worker to handle. Git smart HTTP independently requires a live
attempt and closes access when a result is recorded.

Host9 and Host10 use separate namespaces. Configure Host10 with a new
namespace ID and name and keep `installation_routing = true` if the Host9
binding routed the installation. Startup reinstalls the active authored
routing artifact on `aq.routing` and issues its grant in the new namespace.
Host10 serves the exact Host9 `work.read` contract for that artifact.
If routing was installed through `[[playbooks.deployments]]`, carry its
artifact, scope and `aq.routing` binding into the new namespace with a new
grant ID. Pending Host9 runs stay in their original namespace.

## 8. The policy itself (E0.8–E0.9)

- **Source:** a Markdown document, **Settings › Delivery**. It is authored and
  activated like routing (spec 0013): saving starts the authoring agent, which
  assembles the artifact with the 2.3.0 commands, checks it, and saves it. The
  artifact is activated at once and the previous one is kept for rollback.
  Authoring questions go to the supervisor, not to a panel.
- **Scope:** one document for the installation. A project that needs a
  different rule says so in the document ("for aq-dashboard, also require …");
  the playbook reads `project_id` from the facts. (This is routing's pattern.
  See §11.)
- **The first policy, written by the owner and Claude for Epic 0:**
  1. On `result_ready`: if the outcome is not success, or nothing changed, hold
     and notify the supervisor.
  2. Publish the branch and open or find the PR.
  3. Await checks, up to 30 min.
  4. Passed: merge (`merge`), then `work.complete`.
  5. Failed or conflict, with `returns` below 3: `work.return`, with
     `update_from_main` on a conflict.
  6. Otherwise: hold and notify the supervisor.
  7. Timeout: end the run, so the next `result_ready` looks again.

  The numbers live in the document, not in code.

## 9. Tasks, in dependency order

Every task merges to `main` on its own, behind the compile-time feature
`delivery_playbook` in each repository it touches. New code is additive, with no
else branch. Where behaviour changes (§5.3, §7), the old path stays under
`not(feature)` until step 11. CI builds with the feature off and on.

| # | Task | Repos | Depends on |
|---|---|---|---|
| 1 | Follow main: `repo.retain_ref`, the sync loop and freshness Settings, named-ref policy accepts sync handles, recorded head no longer outranks GitHub (§4) | aq-git, aq-server | — |
| 2 | Git endpoint with attempt grants, quarantine checks and push receipts (§5.1) | aq-git | — |
| 3 | Git grant in the offer, proxy route, report names the push receipt, onboarding prefix and migration (§5.2) | aq-server | 2 |
| 4 | Node clones from and pushes to central aq-git; returned tasks start from their branch, with `update_from_main` (§3, §5.3) | aq-worker | 3 |
| 5 | GitHub App adapter: open or find, checks, mergeability, merge, close, delete; vault configuration (§6.3) | aq-server (`aq-delivery`) | — |
| 6 | Host catalog 2.3.0: `result_ready` emission, the §6.2 commands, the extended `work.read`, `notes.json` docs, the playbook-side contract (§6) | aq-playbooks, aq-server | 1, 3, 5 |
| 7 | Settlement records facts only; deletions under the flag; release guards accept `work.complete` (§7) | aq-server | 6 |
| 8 | Settings › Delivery document, built-in `delivery` authoring record, dashboard editor reusing the routing editor, questions to the supervisor (§8) | aq-server, aq-dashboard | 6 |
| 9 | Write the first policy; pilot on the scratch project on the live stack | Settings, live stack | 4, 7, 8 |
| 10 | Turn on for the six AQ projects. Needs green, fast CI first (below) | live stack | 9 |
| 11 | Remove the flag and the old paths: bundle transfer, the old settlement, the repin script (owner confirms first) | all | 10 |

**Prerequisites for step 10, outside this spec:** the server test job within
its 10-minute budget; the two flaky tests fixed
(`native_cancel_owner_validation_attempt_replay_and_concurrency`,
`inspection_discovers_actual_admissions_runs_and_public_timeline`); the
dashboard's Playwright checks green on `main`; the GitHub App created and
installed on the six repositories (owner).

**Filing:** an attempt's wall-time ceiling on the AQ projects is 15 minutes
(binding `ceilings.wall_time_ms = 900000`). Each row is therefore filed as
several AQ tasks of one change each, each with its own acceptance test, against
this spec. Until step 9 is live, Claude takes each result out of AQ, opens the
PR, and merges it once checks pass.

## 10. Acceptance

1. A merge made on GitHub is offered to the next task within the main-sync
   interval, with no restart.
2. A task's result arrives in central aq-git as `refs/heads/aq/<task-id>` by
   push, with no bundle transfer. The quarantine checks refuse a push that
   fails `fsck`, does not descend from the base, or targets another ref.
3. On the scratch project, with the §8 policy active:
   - a passing change is published, its PR opened, checks awaited, merged,
     and its branch deleted;
   - the task completes, and the next task starts from the merged `main`.
4. A failing check sends the task back with the check names. The repair attempt
   starts from the task branch and updates the same PR. After 3 returns the
   task is held and the supervisor gets one message.
5. A conflict is sent back with `update_from_main`. The agent receives conflict
   markers, and the repaired branch merges.
6. With no delivery playbook active, a result waits, and the task says so. No
   code path completes, publishes or reworks it.
7. Changing the Settings document changes the behaviour, for example the return
   limit or the merge method, with no code change and no restart.
8. `grep` on origin/main after step 11 finds no `default_policy`,
   `aq.default-completion`, `PublicationPolicy` or source-transfer route.

## 11. Defaults taken (2026-11-18; the owner may revise them)

1. One installation-wide delivery document (routing's pattern, §8). Per-project
   documents, as v1 activated its train per project, can follow if wanted.
2. "All gates" includes the human review requirement at offer time (§7). A
   policy that wants a person to approve uses `decision.request`.
3. The first policy merges with `merge`, as the AQ repositories' history does
   (`Merge pull request #…`), not `squash`.
