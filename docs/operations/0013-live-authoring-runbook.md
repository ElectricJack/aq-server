# Live acceptance runbook: playbook authoring and routing on aq-ms1

**Status:** written 2026-11-13 on `feat/internal-work` at `be84c01a`. **Not
executed.** Nothing in this document has been run against `aq-ms1`.

**Changed by [spec 0014](../specs/0014-roles-and-profiles.md) (2026-11-16).**
The Settings routes named below moved: the routing text is read and saved at
`GET`/`PUT /api/settings/routing-document` (`{markdown, revision}`, with
`expected_revision` and `operation_id`); the routing switch is at
`/api/settings/routing`; the structure route
with its `parents` and `assignments` is gone, and no save takes an
`expected_catalog_revision`. A level's id is the router's class and its
`description` is what authoring reads. Internal-work profile ids are
intelligence profile ids (`simple-low` … `deep-high`). Steps 1.7, 5.1, 5.2 and
5.3 must be re-derived before this runbook is executed.

**Do not start yet.** At `be84c01a` the branch still vendors the interim
engine `aq-playbooks-e7df379`, which must not reach a live playbook namespace
([note](../implementation/playbook-authoring-B1-interim-vendor.md),
[status](../status.md)). This runbook is for the finished stack. The tasks
that had not landed when it was written are listed in "Steps to confirm" at
the end. Each affected step is marked **(confirm …)**.

Sources: plan `docs/superpowers/plans/2026-11-13-playbook-authoring.md`
(Task F2), specs [0011](../specs/0011-routing-policy.md),
[0012](../specs/0012-internal-work.md), [0013](../specs/0013-playbook-authoring.md),
[architecture 0006](../architecture/0006-records-are-rolling.md),
[internal work operations](internal-work.md), [row retention](row-retention.md).
Commands follow the last live rollout, whose runbook is on the owner's
machine at `~/.local/share/aq-ms1/internal-work-rollout-20261113T045801Z/RUNBOOK.md`
("the 0119 runbook" below).

## Rules

- The owner approves each numbered section before it runs. No `aq-ms1-*`
  container is touched outside an approved step.
- Run one step at a time. Stop at the first check that fails.
- Never print a login file, a token or a database password. Never read
  `/proc/*/environ` in the internal-work container.
- Docker Desktop on this machine sometimes returns empty output from
  `docker exec`. For a read, run it again. Never repeat a write because its
  output was empty; check its effect with a read.
- Migrations 0119 and 0120 are applied on the live database and frozen. After
  step 4.3 migrations 0121 to the newest are frozen too.
- Nothing is pushed by this runbook.

## 0. Shell setup (every new shell)

```bash
B=/home/jkern/.local/share/aq-ms1
R=$B/playbook-authoring-rollout-$(date -u +%Y%m%dT%H%M%SZ)   # first shell only; reuse the same R afterwards
mkdir -p "$R" && chmod 700 "$R"
API=http://127.0.0.1:28080
SRV_IMG=   # image id of the new server image (step 1.3)
SUP_IMG=   # image id of the new internal-work host image (step 1.3)
umask 077
q() { "$B/psql.sh" -v ON_ERROR_STOP=1 "$@"; }
post() { curl -fsS -X POST -H 'Content-Type: application/json' -H 'X-AQ-Request: 1' "$API$1" ${2:+--data "$2"}; }
```

`psql.sh` is the stack's database shell. `post` is the helper of the 0119
runbook.

## 1. Preconditions

| # | Action | Expected | Rollback |
| --- | --- | --- | --- |
| 1.1 | Confirm every plan task is merged: engine A1–A7 and C5, server B1 (final re-vendor at 0.2.0) to D4, dashboard E1 and E2, F1. `ls vendor \| grep aq-playbooks` in the tree to deploy. | One engine tree, pin `aq-playbooks-source-0.2.0.json`. No `aq-playbooks-e7df379`. `docs/status.md` no longer carries the "Do not deploy" banner. | None needed. |
| 1.2 | Build deploy branches the way the last rollout did: a local branch that merges the feature branch with what the live stack runs, for aq-server and aq-dashboard. Check the frozen migrations: `sha256sum crates/aq-server/migrations/0119_internal_work.sql crates/aq-server/migrations/0120_routing.sql`. | The two digests recorded in the 0119 runbook (`0f33f8ca…`, `f5678db5…`). A different digest: stop. | Delete the local deploy branch. |
| 1.3 | Build the images from the deploy tree: the server image, and the internal-work host image (the supervisor host image with the engine CLI, Task C3). Build the dashboard bundle with `pnpm build && node scripts/bundle-manifest.mjs dist --official`. **(command to confirm: the exact image build commands and tags; the last rollout used per-rollout Dockerfiles described in its `PREPARATION-REPORT.md`)** | Two image ids, both labelled with the deploy commit. Put them in `SRV_IMG` and `SUP_IMG`. | Remove the new image tags. |
| 1.4 | The stack is idle: `curl -fsS "$API/api/v1/swarm" \| python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["server"]["running_agents"])'` | `0`. If not, wait. | None. |
| 1.5 | Record the starting point: `q -c "SELECT value FROM meta WHERE key='state_format_version'"` and `curl -fsS "$API/api/playbooks/status" > "$R/playbooks-status-before.json"` | `120`. The status file names the namespace and its generation. | None. |
| 1.6 | Back up the live database (the engine schema `aq_playbooks` is inside `agent_q`): | | |

```bash
docker exec aq-ms1-postgres-1 sh -c 'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; exec pg_dump -h 127.0.0.1 -U agent_q -d agent_q -Fc' > "$R/database-before.dump"
chmod 600 "$R/database-before.dump"; sha256sum "$R/database-before.dump" > "$R/database-before.dump.sha256"
docker exec -i aq-ms1-postgres-1 pg_restore -l < "$R/database-before.dump" | grep -c 'TABLE DATA'
```

Expected: a dump of tens of MB and a `TABLE DATA` count in the hundreds. An
empty dump or a `pg_restore -l` error: stop. This dump is the only way back
after step 4.3.

1.7. Save copies of what will be edited: the server's vault config (the file
holding `[playbooks]`), the Compose override
`$B/intelligence-settings.compose.yaml`, and the Settings documents:

```bash
curl -fsS "$API/api/settings/routing-document" > "$R/routing-document-before.json"
curl -fsS "$API/api/settings/internal-work" > "$R/internal-work-settings-before.json"
curl -fsS "$API/api/settings/routing" > "$R/routing-before.json"
```

Expected: routing `"enabled": false`. `routing-document-before.json` holds the
owner's current routing text; it is the copy to restore from.

## 2. Rehearsal on a copy of the live database

Purpose: see the migrations and the first engine open succeed, and time
them, before the live database is touched. The live stack is not changed in
this section.

2.1. Restore the dump into a throwaway PostgreSQL that is not part of the
`aq-ms1` Compose project (its name must not start with `aq-ms1-`), on its own
port. It needs the same PostgreSQL major version, the roles the dump refers
to (`agent_q`, and the engine role `aq_playbook`, see
`scripts/dev-playbook-db.sh`), then:
`pg_restore -h 127.0.0.1 -U agent_q -d agent_q --single-transaction --exit-on-error < "$R/database-before.dump"`.
**(command to confirm: creating the throwaway instance and its roles)**
Expected: `SELECT value FROM meta WHERE key='state_format_version'` is `120`
in the copy. Rollback: remove the throwaway container and its volume.

2.2. Start the new server image against the copy, with a copy of the vault
(config, the playbook engine connection file pointed at the copy, and the
engine keyring, without which the engine cannot open its state), on ports
other than 28080/28081. `scripts/ms1-demo.sh` runs a second stack with
`AQ_MS1_PROJECT`, `AQ_MS1_ADMIN_PORT`, `AQ_MS1_WORKER_PORT` and
`AQ_MS1_GRAPH_PORT`. No worker, no internal-work host and no login is given
to the rehearsal. **(command to confirm)** Rollback: stop and remove the
rehearsal stack.

2.3. Migrations. Expected in the copy:
`SELECT value FROM meta WHERE key='state_format_version'` is the newest
migration number (128 at `be84c01a`), and `public.schema_migrations` has one
row for each of 121 to the newest. Record how long the start took.

- 0121 turns the routing switch off and drops `routing_policies`. 0122 and
  0123 add the authoring and transcript tables. 0125 adds `held_reason`.
- 0124 and 0126 set `lock_timeout = '5s'`. If a lock is not granted in 5 s
  the migration fails with `55P03`, its transaction rolls back and the server
  exits. Start it again; that is the retry. 0126 drops the six counter
  columns of `playbook_namespaces` and builds five indexes.
- 0127 replaces a trigger function and 0128 creates a function. At
  `be84c01a` these two files set no lock timeout of their own. **(confirm
  against the final migration files)**

2.4. Engine open. `curl -fsS http://127.0.0.1:<rehearsal port>/api/playbooks/status`.
Expected: 200 with `automatic_driver_state`, not 503. Note the time from
server start to the first 200.

- The first open of the old namespace runs the engine's own migrations and
  analyzes its tables that have no planner statistics.
- It then drains the runs that finished under the old engine: up to 50 runs
  and 500 old entries per commit, while new work is served. The engine's
  measurement on a 1,000-run namespace was 21 commits and 25.8 s in all, the
  slowest commit 3.7 s.
- The open must fit `[playbooks.engine_timeouts] open_ms` (default 30 000)
  and each commit `statement_ms` (default 30 000). If the open is refused the
  server still starts, playbook routes answer 503 `playbooks_unavailable`
  with the failed step, and the open is retried with a backoff.

2.5. Drain finished. In the copy, for the namespace id in
`playbooks-status-before.json` (the engine names it `aq/<namespace id>/<generation>`):

```sql
SELECT count(*) FROM aq_playbooks.disposal_tombstones
 WHERE namespace = 'aq/<namespace id>/<generation>' AND kind IN ('event','run','intent');
SELECT count(*) FROM aq_playbooks.run_history;
```

Expected: the first count reaches 0 and `run_history` holds the old finished
runs. Record the time it took. If any commit came close to `statement_ms`,
raise `statement_ms` and `open_ms` in the live config before step 4.

2.6. Record in `$R/rehearsal.txt`: migration time, time to first open, time
to drain, the slowest step. Then remove the rehearsal stack and database.

## 3. The namespace moves to `host_v9` (catalog 2.2.0)

**State the engine writes at 0.2.0 cannot be read by the old engine.** Once
the new server has opened the live namespace (step 4.3), going back to the
old server means restoring the database from the step 1.6 dump. Everything
written after the dump is lost. The server migrations are forward-only as
well.

What can be undone without a restore is the profile change alone, on the
same new binary (3.4).

3.1. Before the server switch. Record the current profile and generation
from the saved vault config. The plan expects `host_v8`; the 0119 rollout
recorded `human_review_v2` on the live stack. **(confirm the live profile)**
Record the live runs: `curl -fsS "$API/api/playbooks/inspection/runs" > "$R/runs-before.json"`.

3.2. First switch the server with the playbook config unchanged (step 4.3).
Expected: the old profile keeps running on the new engine.

3.3. Then open a `host_v9` namespace, before routing is ever on. **A
namespace never changes its profile or its generation**: there is no move.
`host_v9` is a new namespace beside the old one.

1. `post /api/playbooks/stop '{}'`. Expected: `{"disposition":"driver_stopped","queued_messages":"retained"}`.
2. Wait until no run of the old namespace waits on an effect, at most
   30 min. A run still live after that is not continued. Write its id,
   binding and step into `$R/runs-not-continued.txt`.
3. Edit `[playbooks]` in the vault config:
   - `profile = "host_v9"`;
   - `[playbooks.namespace]`: a **new `namespace_id`** (a fresh UUID), a new
     `name`, `generation = 1`; workspace, project and owner as before;
   - `installation_routing = true` (the routing commands then serve the
     tasks of every project on the automatic router; refused for any other
     profile);
   - `deployments = []`, or deployments whose grant scope has no
     recipients. A `human_review_v2` mail deployment cannot be carried over.
   Recreate the server (the command of 4.3).
4. Expected after: `GET /api/playbooks/status` shows the new namespace and
   a `registered_commands` list that includes `work.read`, `work.route`,
   `work.unroutable` and `judgment.assess`; `automatic_driver_state` is not
   `retrying`; `held_events` is empty. `post /api/playbooks/coding/default '{}'`
   re-installs the default pipeline. Opening the old namespace with the new
   profile instead is refused with a text that says exactly this (playbook
   routes answer 503 with it; the rest of the server runs).

Grants and artifacts do not carry over: they belong to the old namespace.
The old namespace's rows are kept and its unfinished runs are no longer
driven. A `human_review_v2` namespace's review playbooks do not run in a
`host_v9` namespace; one server has one `[playbooks]` namespace.

3.4. Rollback of 3.3 only: turn routing off (5.7 rollback), restore the
saved vault config (old profile, old `namespace_id`, name and generation)
and recreate the server on the same new image. The old engine namespace was
not deleted, so it resumes. Source grants of the new namespace stay unused.

3.5. Configuration keys added on this branch. All are optional; the
defaults are given.

| Table | Keys (default) | What it bounds |
| --- | --- | --- |
| `[playbook_authoring]` | `deadline_ms` (1800000), `max_turns` (200), `outcome_bytes` (4194304), `case_transitions` (40000), `regression_cases` (200), `check_deadline_ms` (60000), `keep_finished_runs` (50), `facts_restarts` (3), `poll_ms` (2000) | One authoring run: the agent's time and turns, the size of its outcome, the server's own re-run of the cases, how many cases of the previous playbook a save may leave out and still have run again as regression cases (each batch has the `case_transitions` budget), how many finished runs a source keeps, how often authoring restarts by itself when Settings changed under it, and how often the server looks for finished runs. A value internal work would refuse is refused at start. |
| `[playbooks]` | `request_bytes` (262144; 1024 to 1048576) | The largest request to the engine owner. An install carries its whole artifact: raise it when the routing artifact does not install. |
| `[playbooks]` | `installation_routing` (false) | See 3.3. |
| `[playbooks.retention]` | `keep_finished` (10000), `delete_batch` (500) | Rolling deletion of the server's finished playbook bridge rows, per table. |
| `[playbooks.engine_retention]` | `event_replay_ms` (86400000), `admission_replay_ms` (604800000), `replay_keep` (256), `history_keep` (10000), `audit_ms` (604800000) | The engine's replay windows and rolling sizes, applied when the engine opens. |
| `[playbooks.engine_timeouts]` | `statement_ms` (30000), `lock_ms` (500), `open_ms` (30000) | One engine statement, one lock wait, and one attempt to open the engine. Raise `open_ms` when the first open of a large namespace is abandoned and retried. |
| `[workers.heartbeat_log]` | `keep` (256), `delete_batch` (4096) | Rolling deletion of the worker heartbeat log. |

Also needed for authoring: an internal work profile for the purpose
`playbook_compilation` that lists a logged-in harness (Claude or Codex) in
Settings > Internal work, and the internal-work host image of 1.3 (it holds
`/usr/local/libexec/aq-playbooks`).

## 4. Deploy order

Server first. Then the internal-work host image. Then workers. The dashboard
after the server.

4.1. Why this order: an older internal-work host claims agent tasks and runs
them as one prompt with tools off, so every host is updated before anything
submits agent work. The server must have migrated (0123) before a host runs
agent work.

4.2. Install the Compose override for the new server image and dashboard
bundle, as the 0119 runbook step C1 did (write the new override to
`$R/compose-after.yaml`, `diff` it against the saved one, install it as
`$B/intelligence-settings.compose.yaml`). Expected: the diff shows only the
server image, its build context and the dashboard bundle path.

4.3. **Server.** Recreate only the server. It applies migrations 0121 to the
newest and opens the engine. **This is the point of no return (section 3).**
Use a copy of the 0119 rollout's `compose.sh` pointed at this rollout's
directory and deploy worktree:

```bash
"$R/compose.sh" after up -d --no-deps --no-build --force-recreate --wait --wait-timeout 300 server 2>&1 | tee "$R/activation.log"
curl -fsS "$API/healthz"; echo
q -c "SELECT value FROM meta WHERE key='state_format_version'"
q -c "SELECT version, applied_at FROM public.schema_migrations WHERE version > 120 ORDER BY 1"
curl -fsS "$API/api/playbooks/status"; echo
```

Expected: only `aq-ms1-server-1` is recreated and becomes healthy; the state
format is the newest migration number; one ledger row per new migration; the
playbook status answers 200. If the server exits on `55P03`, run the `up`
command again. If playbook status answers 503, read its `message` (it names
the step that failed) and see 9.5; the rest of the server is up.
Then check the drain as in 2.5, against the live database.
Rollback: before this step, reinstall the saved override. After it, the
restore in section 11.

4.4. **Dashboard** (after the server). On `aq-ms1` the bundle is mounted
into the server container by the same override, so it switched in 4.3.
Verify with the 0119 rollout's `verify-bundle.py` against the new bundle's
`dashboard-manifest.json`, then open `http://127.0.0.1:28080/` and go to
Settings > Intelligence. Expected: "served bundle verified", and the routing
editor shows the authoring status area (E1). Rollback: point the override at
the previous bundle and recreate the server.

4.5. **Internal-work host.** Replace the container with the new image,
keeping its volumes (config, logins, native, homes, scratch):

```bash
docker stop aq-ms1-internal-work && docker rm aq-ms1-internal-work
```

then the `docker create … "$SUP_IMG" internal /config/internal.json`,
`docker network connect aq-ms1_edge aq-ms1-internal-work` and `docker start`
of the 0119 runbook step D5, unchanged except for the image (the running
container uses `--cpus 1.0`). Then:

```bash
docker inspect aq-ms1-internal-work --format '{{.State.Status}} restarts={{.RestartCount}}'
docker logs aq-ms1-internal-work 2>&1 | tail -5
docker exec aq-ms1-internal-work ls -l /usr/local/libexec/aq-playbooks
docker exec aq-ms1-internal-work /usr/local/libexec/aq-playbooks --help
```

Expected: `running restarts=0`, no log lines, the engine CLI root-owned, and
its help listing the authoring commands. `aq-playbooks` must not be on the
container's `PATH`. Rollback: the same commands with the previous image id;
only safe while no agent work has been submitted.

4.6. **Workers.** Rebuild and restart the worker service only if the stack
includes aq-worker changes. Operator guide:
`aq-worker/docs/operations/install-service.md`. **(command to confirm)**
Expected: the node reports in again on the Agents page. Rollback: the
previous worker binaries.

## 5. Routing document, authoring run, switch

5.1. Give playbook authoring a Claude-first profile (a Codex-first agent
task can wait on the one Codex login). Read
`GET /api/settings/internal-work`, then `POST /api/settings/internal-work`
with a new `operation_id`, the `expected_revision` and
`expected_catalog_revision` just read, the same `default_profile_id`, and
`"overrides": {"playbook_compilation": "<profile id>"}` (the `iw_save`
helper of the 0119 runbook does this). Expected: the saved document shows
the override. The profile must have at least one logged-in candidate, or
authoring fails at once with `no_candidate`. Rollback: save `"overrides": {}`.

5.2. Compare the live routing text with the repo copy. The live copy is
older.

```bash
python3 -c 'import json,sys; sys.stdout.write(json.load(open(sys.argv[1]))["markdown"])' "$R/routing-document-before.json" > "$R/routing-live.md"
diff "$R/routing-live.md" docs/specs/0011-routing-policy-default.md
```

Expected: a diff. Show it to the owner before saving.

5.3. Save the repo copy as the routing document. In the dashboard: Settings >
Intelligence, routing editor, replace the text with the content of
`docs/specs/0011-routing-policy-default.md`, save. By API it is
`PUT /api/settings/routing-document` with `operation_id`,
`expected_revision` (the `revision` of a fresh GET) and `markdown` read from
the repo file. Expected: `{revision, digest}` with a higher revision. The save
creates the `routing` source grant and starts one authoring run (nothing
starts for an empty document, or when neither the text nor what Settings
offers changed). Rollback: save the text of
`routing-live.md` back; that starts another authoring run.

5.4. Watch the run. It may take up to 30 minutes.

```bash
curl -fsS "$API/api/settings/routing"            # {enabled, source: SourceView}
curl -fsS "$API/api/playbooks/sources/routing"                # SourceView
curl -fsS "$API/api/internal-work/runs?purpose=playbook_compilation"
```

In the dashboard the same is under the routing editor ("Writing the
playbook…") with the agent's steps (E2). Expected end states of
`source.latest.status`:

| Status | Dashboard wording | Meaning | What to do |
| --- | --- | --- | --- |
| `running` | "Writing the playbook…" | The agent is working. | Wait. |
| `saved` | "In effect since …" | The playbook is installed and active. | Go to 5.5. |
| `question` | "The playbook needs an answer" | The agent would have had to guess. Nothing changed. | Record the question and its quoted sentences verbatim. Leave it for the owner. |
| `unable` | "This sentence cannot be carried out" | The catalog cannot serve a sentence. Nothing changed. | Record the sentence and reason. Owner decides. |
| `refused` | the reason | A save was refused (cases did not run as stated, or abilities outside the grant). Nothing changed. | Record the reason. Run again with 5.6. |
| `failed` | shown on the internal run | The agent ended without an outcome, or timed out. Nothing changed. | Read the run's attempts. Run again with 5.6. |
| `superseded` | not shown as current | A newer save replaced this run. | Watch the newer run. |

Acceptance 1 passes on `saved`, or on a `question` that quotes only the
"Research is Standard, high effort, unless it is clearly harder" sentence
and/or the "Very high risk work is at least Complex … unless it is narrow
and covered by a test" sentence. Find their current line numbers with
`grep -n -e 'Research is Standard' -e 'Very high risk' docs/specs/0011-routing-policy-default.md`
(the plan's numbers 21 and 51 are from an older copy). Do not edit the
document to make it pass.

5.5. Read the cases table (dashboard, under "In effect since"; in the API,
`source.active.cases`, each with `situation`, `result`, `passed`). Expected:
at least 5 rows, every row passed, the change shown in plain words ("No
change", "Wording only", "Behaviour changed", "Abilities changed"), and no
step id or `sha256:` anywhere. Read each row against the document with the
owner. A row the owner disagrees with is a reason not to switch on.

5.6. Author again without changing the text: `post /api/playbooks/sources/routing/runs '{}'`
(it starts nothing while the text and the offers are those of the playbook
in effect). Roll back to the previous playbook:
the "Roll back to the previous playbook" button, or
`post /api/playbooks/sources/routing/rollback '{}'` (409 when there is no
earlier playbook; a run in flight is superseded).

5.7. Switch routing on, only after a `saved` run and the owner's reading of
the cases:

```bash
post /api/settings/routing '{"enabled":true}'
```

Expected: `"enabled": true`. Without an active routing playbook the switch
is refused. Rollback: `post /api/settings/routing '{"enabled":false}'`.
Off, placement ignores a missing route; routes already written are still
honoured.

## 6. Real-login checks

The tests used fake model CLIs. These four need the real logins.

6.1. **Claude enforces `Bash(aq-playbooks:*)`.** First the flags, help only:

```bash
docker exec --user 10002:10002 -e HOME=/tmp aq-ms1-internal-work /opt/claude/claude --version
docker exec --user 10002:10002 -e HOME=/tmp aq-ms1-internal-work /opt/claude/claude --help | grep -E -- '--tools|--allowedTools|--disallowedTools|--permission-mode'
```

Expected: the version mounted, and each flag listed (`--max-turns` is
accepted but not listed in 2.1.323). Then read the transcript of the 5.4
run: `curl -fsS "$API/api/internal-work/runs/<run id>"`. Expected: every
shell call is `aq-playbooks …`; file tools are only Read, Write, Edit, Glob,
Grep. Then the probe: one agent run on the Claude login that is asked to run
`aq-playbooks --help && id`, `aq-playbooks --help; id`, `aq-playbooks --help | cat`,
`aq-playbooks $(id)` and `aq-playbooks --help > /tmp/x`. Expected: each is
refused by the CLI and appears in the transcript as a refused call; `id`
never runs. **(command to confirm: submitting a probe agent run)** If any
compound command runs: turn routing off, clear the `playbook_compilation`
override so no agent work starts, and stop.

6.2. **Codex sandbox inside the container.** Temporarily make the
`playbook_compilation` override a Codex-first profile (5.1), start a run
(5.6), and while it runs:

```bash
docker top aq-ms1-internal-work
```

Expected: the Codex processes belong to uid 10002. The run's transcript
shows `aq-playbooks` calls that succeeded, which means the sandbox
(bubblewrap with user namespaces) started and the env-clearing shim ran the
real binary. If the sandbox cannot start, the expected result is a fast
failure of the Codex candidate with a clear attempt message, and the next
candidate runs. Do not turn on a weaker sandbox mode. Afterwards restore the
Claude-first override. Rollback: the override save of 5.1.

6.3. **The timeout message.** It cannot be forced safely. If a candidate
runs out of time, its attempt must read "`<harness>` did not finish within
N s" (agent work), not "runner did not report". Read attempts with
`q -c "SELECT id, status, reason, detail FROM internal_tasks WHERE purpose='playbook_compilation' ORDER BY created_at_ms DESC LIMIT 5"`.
If none timed out, record "not exercised".

6.4. **No token in a transcript.** A first pass by token prefix (a prefix is
not a secret):

```bash
q -c "SELECT count(*) FROM internal_task_transcripts WHERE events::text LIKE '%sk-ant-%'"
q -c "SELECT count(*) FROM internal_tasks t WHERE t::text LIKE '%sk-ant-%'"
```

Expected: `0` and `0`. Then the exact check for both logins: no stored
transcript, outcome or failure detail contains the Claude token or any token
of the Codex login file. The secret must reach the query without appearing
in a command line, a log or the terminal. **(command to confirm)** A hit:
stop agent work (clear the override), tell the owner, and treat the login as
exposed.

## 7. First routed tasks

Routes apply only in a project whose binding uses the automatic router.
Create both tasks in such a project, from the dashboard (Work > new task).

| Task | Example | Expected route per the routing document |
| --- | --- | --- |
| Simple | A chore: "Chore: update the copyright year in the README footer." | Level Simple, high effort ("Chores and syncs are Simple, high effort"). Low risk. No restriction to Claude or Codex. |
| Complex | "Write the implementation plan for moving the session table to a new schema, including the database migration." | Level Complex, high effort (plans are Complex). High risk (database migrations), so Claude or Codex only; planning goes to Claude first, Codex only when Claude cannot take it. Not Deep. |

On `aq-ms1` only harnesses that Settings offers can appear in a route. At
the 0119 rollout only the Claude harness was qualified, so "who may take it"
may list Claude alone.

7.1. Where to see the judgment: the task's inspector shows its routing
classification inline (the internal run: what was asked, which model
answered, how long, the answer). In the API:
`curl -fsS "$API/api/internal-work/runs?affected_kind=task&affected_id=<task id>"`.
Expected: one `routing_classification` run, `answered`. A task whose kind
needs no judgment may have none.

7.2. Where to see the route: the task's inspector (level, effort, who may
take it, and the reasons), and `curl -fsS "$API/api/tasks/<task id>/routes"`
(every route the task has had, newest first). Expected: within about two
minutes each task has one route matching the table, with a reason recorded.
Before that the task shows "Waiting for routing" with how long it has
waited.

7.3. What "unroutable" looks like **(confirm after D3)**: the task stays
unrouted and visible with the reason on it. The server records a failed
internal task for the task (`no_candidate` when nothing allowed could ever
take it; `deadline_exceeded` "unrouted for 10 minutes"; `provider_error`
"the routing playbook failed for this task"). The project's supervisor is
messaged once. If it cannot fix it, the task appears under "Needs your
attention" and the item opens the task. A later route resolves it. A task
that only waits because nothing allowed is free right now is a wait, not a
failure. A single classifier failure raises nothing: the task is still
routed, at Standard, high effort, on Claude or Codex, with the risk marked
as assumed.

Rollback for this section: switch routing off (5.7). The owner can also set
or clear a task's route by hand.

### 7.1 Driver limits (a wave of ready tasks)

One drive quantum admits, collects, delivers and ticks within bounds that are
Settings, the `driver` object of `GET/PUT /api/settings/profiles/limits`. They
are read at the start of every quantum, so a change applies at once. A quantum
that reaches a bound with work left is followed after 10 ms, not after
`drive_interval_ms`.

| `driver` field | Default | Range | What it bounds, per quantum |
| --- | --- | --- | --- |
| `admissions` | 8 | 1–16 | Keyed admissions delivered to the engine. |
| `events` | 8 | 1–16 | Events of each internal kind collected, and expiries. |
| `outbox_events` | 32 | 1–64 | Outbox events delivered to the engine. |
| `routing_asks` | 32 | 1–1000 | Waiting tasks asked about (`aq.work.route_needed`). |
| `announcements` | 16 | 1–1000 | Results and base changes announced. |
| `delivery_operations` | 8 | 1–8 | Delivery operations collected. |
| `tick_transitions` | 256 | 1–4096 | Run transitions of the one tick, for the whole server. |
| `tick_budget_ms` | 15 000 | 1 000–120 000 | The tick starts no new transition after this long. |
| `route_error_retries` | 3 | 0–20 | Routing runs retired and the task asked about again, per task version. |

A command a transition creates is handed to the host in the same tick, before
the next transition, so its `deadline_ms` is spent in the host call and not
waiting for the next quantum. A routing run blocked because a `work.route` or
`work.unroutable` command was refused for an expired deadline (nothing was
done) is retired by the delivery recovery, and its task, if still ready and
unrouted, is asked about again at once, up to `route_error_retries` times for
the task's current version. After that the task waits as before and is raised
to the owner after ten minutes. Only a delivery profile (host_v10 and later)
runs this recovery. The host's refusal reason is logged as
`playbook host rejected a command`; the engine's `operation_rejected` receipt
carries no reason.

## 8. Rolling records

Nothing here refuses work when it is full. Each store keeps its newest
records and deletes older ones itself.

| Key (vault `config.toml`) | Default | What it does |
| --- | --- | --- |
| `[playbooks.retention] keep_finished` | 10 000 | Finished rows kept per bridge table and namespace generation. |
| `[playbooks.retention] delete_batch` | 500 | Most rows one drive quantum deletes; one table per quantum, in turn. |
| `[workers.heartbeat_log] keep` | 256 | Newest heartbeat operations kept per node. |
| `[workers.heartbeat_log] delete_batch` | 4 096 | Most heartbeat rows one sweep deletes. The sweep runs every 10 minutes. |
| `[playbooks.engine_timeouts] statement_ms` | 30 000 | One engine statement. |
| `[playbooks.engine_timeouts] lock_ms` | 500 | One engine lock wait. At most `statement_ms`. |
| `[playbooks.engine_timeouts] open_ms` | 30 000 | One attempt to open the engine. |

All are positive with no upper bound. A change needs a server restart.
Engine history keeps the newest 10 000 finished runs (`history_keep`, an
engine default the server does not set).

8.1. Bridge tables stay flat. Run now, after an hour, and after a day:

```bash
q -c "SELECT 'outbox', count(*) FROM playbook_outbox UNION ALL SELECT 'admissions', count(*) FROM playbook_admissions UNION ALL SELECT 'effects', count(*) FROM playbook_effects UNION ALL SELECT 'cancellations', count(*) FROM playbook_cancellations UNION ALL SELECT 'host_operations', count(*) FROM playbook_host_operations UNION ALL SELECT 'health_observations', count(*) FROM playbook_health_observations UNION ALL SELECT 'finding_notifications', count(*) FROM playbook_finding_notifications UNION ALL SELECT 'finding_episodes', count(*) FROM playbook_finding_episodes"
```

Expected: each count levels off at about `keep_finished` plus work in
progress. Below 10 000 rows a table simply grows; that is correct.

8.2. Heartbeat log:

```bash
q -c "SELECT count(*), count(DISTINCT node_id) FROM profiled_presence_operations"
docker logs aq-ms1-server-1 2>&1 | grep rows_deleted | grep profiled_presence_operations | tail -3
```

Expected: the count falls by up to 4 096 every 10 minutes until it is near
256 per node, then stays there. **An existing backlog drains at
`delete_batch` per 10 minutes**: a backlog of N rows takes about
N ÷ 4 096 × 10 minutes (one million rows: about 41 hours). To drain faster,
raise `[workers.heartbeat_log] delete_batch` and restart the server.

8.3. Engine history and state:

```bash
q -c "SELECT relname, pg_size_pretty(pg_total_relation_size(oid)) FROM pg_class WHERE relnamespace='aq_playbooks'::regnamespace AND relkind='r' ORDER BY pg_total_relation_size(oid) DESC"
```

Expected: after the drain the state table is small and stops growing with
the number of routed tasks; `run_history` grows to 10 000 runs and then
stays. Tick time should stay flat as well: record
`curl -fsS -o /dev/null -w '%{time_total}\n' "$API/api/playbooks/status"`
and the slowest drive step in the server log before and after the first day.

## 9. The first day: what to watch and what to tune

9.1. **Authoring cost and time.** `curl -fsS "$API/api/internal-work/runs?purpose=playbook_compilation"`:
provider, model, elapsed time and usage per run. Deadline 30 min; each later
candidate is left at least 300 s. Tune: the `playbook_compilation` profile
override (a stronger or faster model, Claude first). Repeated `failed` runs
with a timeout message mean the profile is too slow.

9.2. **Judgment latency.** `curl -fsS "$API/api/internal-work/runs?purpose=routing_classification"`:
elapsed time per task. A ready task waits for it; routing is re-emitted at
most every 120 s and a task unrouted for 10 minutes is a failure. Tune: the
`routing_classification` override (a fast, cheap profile), or the default
profile.

9.3. **Held events.** `curl -fsS "$API/api/playbooks/status"` lists
`held_events` (at most 25, newest first: event id, producer, creation time,
reason `stale`, `uncertain`, `refused` or `invalid`). Held admissions:
`curl -fsS "$API/api/playbooks/inspection/admissions"` (`held_reason`).

- Put a held event back: `post /api/playbooks/events/<event id>/retry '{}'`.
  Expected: `"disposition": "pending"`. An event the engine had accepted
  resumes its fan-out; it never starts a second run.
- Hold a queued event that blocks the page (a quantum failing with "cannot
  decrypt queued event <id>"): `post /api/playbooks/events/<event id>/hold '{}'`.
  Expected: `"disposition": "held"`; delivery goes on past it. Undo with
  `/retry`.
- Retry a held admission: `post /api/playbooks/admissions/<id>/retry '{}'`.
  Its answer carries a warning that the work may run again.

9.4. **The drive.** In the status read, `automatic_driver_state` `retrying`
with an `automatic_driver_reason` means the last quantum failed or was
partial. It retries by itself with a backoff. A reason that stays for many
minutes needs a look.

9.5. **503 `playbooks_unavailable`.** Every `/api/playbooks/…` route answers
`{"error":"playbooks_unavailable","message":"playbooks unavailable: <reason>"}`
while the engine cannot be opened. The rest of the server works, the open is
retried, and the routes come back by themselves. While it lasts nothing is
routed: with routing on, ready tasks show "Waiting for routing". If it lasts,
read the reason (it names the failed step), fix the cause (database, engine
role, keyring, or raise `open_ms`), or switch routing off so tasks are placed
without routes. This state is not on the dashboard attention list (see 10).

9.6. **Other signs.** Internal-work host log lines (`claim failed`,
`run failed`, `a transcript was not recorded`, `login refresh was not copied
back`); tasks "Waiting for routing" longer than a few minutes; "Needs your
attention" items for routing; the `scratch` volume's size.

## 10. Known gaps accepted for now

- **The bad-item skip covers only the graph collector.** A graph source item
  that is not a valid event is queued held `invalid` and later items flow.
  The settlement and schedule collectors are unchanged: a bad item there
  still blocks its own source until it is fixed.
- **The engine-unavailable reason is not on the dashboard attention list.**
  It is visible only in the 503 message, the status read and the server log.
- **`playbook_grants`, `playbook_work_keys` and
  `playbook_integration_receipts` still grow.** Rolling deletion does not
  cover them.
- Internal-work runs and transcripts are not deleted yet (target: newest
  10 000, none older than 30 days).
- Authoring runs are not deleted yet (target: newest 20 per source).
  **(confirm after C4)**
- Engine retention values (`history_keep`, `replay_keep`, the replay
  windows) are engine defaults, not Settings. The `[playbooks.retention]`,
  `[workers.heartbeat_log]` and `[playbooks.engine_timeouts]` values are
  config file keys, not dashboard Settings.
- The engine's admin operations (rebase the retention clock, lower the
  stale floor) are not exposed by the server.
- With no request times, a re-sent request that reaches the engine after it
  has forgotten the key runs again.
- The engine does not guard the size of work in progress: step time grows
  with the number of live runs.
- Redaction replaces exact login secrets. A secret an agent splits, encodes
  or reverses is not matched.
- A Codex agent's shell can read its own Codex login file. The provider
  sees what the agent reads.
- Nothing bounds what an agent writes into its workspace in 30 minutes.
- Usage-limit and login failures are recognised by message wording that has
  not been checked against a live failure. Run-time failover between
  providers has been shown only in tests.
- Other sources than routing have an API but no dashboard screen.
- `routing_policies` is dropped by 0121; `route_needed_emissions` stays.
- One shared Codex login file is copied from the owner's own; two users of
  one login can lose a refresh.

## 11. Rollback after the server switch (database restore)

Use only with the owner's decision. It loses everything written after
step 1.6: task updates, results, mail, Settings saves, internal runs, the
authored playbook.

1. Quiesce (1.4). Switch routing off.
2. `docker stop aq-ms1-internal-work && docker rm aq-ms1-internal-work`,
   then recreate it from the previous image (0119 runbook D5).
3. Reinstall the saved Compose override and the saved vault config.
4. `docker stop aq-ms1-server-1`. Drop what migrations 0121 to the newest
   created and the dump does not contain (tables `playbook_sources`,
   `playbook_authoring_runs`, `playbook_source_grants`,
   `internal_task_transcripts`; function `playbook_run_finished`; the
   engine's new `aq_playbooks` tables), then restore:
   `docker exec -i aq-ms1-postgres-1 sh -c 'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; exec pg_restore -h 127.0.0.1 -U agent_q -d agent_q --clean --if-exists --single-transaction --exit-on-error' < "$R/database-before.dump"`.
   **(command to confirm: the exact DROP list, checked against the final
   migrations and a rehearsal restore in section 2)**
5. Start the previous server image with the `before` mode of `compose.sh`.
   Expected: healthy, state format `120`, Settings equal to the saved
   documents.

## Steps to confirm

Marked in the text. Collected here:

| Step | What to confirm | When |
| --- | --- | --- |
| 1.3 | Image build commands and tags | At preparation |
| 2.1, 2.2 | Throwaway PostgreSQL with roles; starting the new server against the copy | At preparation |
| 2.3 | Lock timeouts of 0127 and 0128 (none in the files at `be84c01a`; 0124 and 0126 have 5 s) | After the final migrations |
| 3.1 | The live profile before the move (`host_v8` or `human_review_v2`) | Read-only pre-check |
| 4.6 | Worker rebuild and restart | At preparation |
| 6.1 | How to submit a probe agent run | After C2/C6 |
| 6.4 | Exact-token search without exposing the token | At preparation |
| 7.3 | Unroutable reasons, supervisor message, attention item | After D3 |
| 10 | Authoring-run deletion state | After C4 |
| 11 | DROP list for the restore | After the final migrations |
