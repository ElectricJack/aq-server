# 0021: Every hold reaches someone who can act on it

Date: 2026-11-22. Contract for aq-server and aq-playbooks. Status: approved by the
owner (design), spec draft for review. Builds on 0014 (roles and profiles),
0016 (delivery by playbook) and the supervisor authority model.

## 1. Intent

Owner, 2026-11-22, after task `e931dfce` (aq-worker) sat held for hours on a
retry budget and nothing reached them:

- "I think we increase the time budget, but also shouldn't each attempt start
  the timer over?"
- "I absolutely want this hold (and really all holds) notifying the project
  supervisor, and then it's up to the project supervisor to either try to
  resolve it itself, or escalate to the global supervisor to determine if it
  should be escalated to a human."
- On the CLI read that made the supervisors look broken: "can we get rid of
  the supervisor path you thought was broken? I don't want to keep misleading
  code."

## 2. What happened (2026-11-22)

1. Task `e931dfce`'s first attempt was offered at ~08:04 with an execution
   deadline of 12:14. The replacement check charges each attempt its whole
   admitted window ("each granted attempt is billed its full admitted window",
   `aq-profiled-state/src/replacement.rs`), so one attempt cost ~250 minutes
   against `max_lineage_sandbox_ms` = 240 minutes, though it ran a few
   minutes. The live `sandbox` profile's `wall_time_ms` is 14,400,000 (4 h), so
   any task that fails once is held.
2. The project supervisor was told. It ran at 08:33 and 15:39, investigated,
   declined to retry, and wrote "Owner action needed: inspect the failed step
   on PR #105" in its own conversation. That message reached nobody: it was
   not sent to the global supervisor, and owner notifications have no external
   route (`external_state: no_external_route`).
3. It could not see why the check failed:
   - 08:26–08:33: the server answered 949 requests that hour with
     `503 overloaded`, the supervisor's inspection reads among them. Cause not
     yet diagnosed.
   - 15:39:14: three `404 not_found` refusals. The server's refusal log
     records no request path, so the route is unknown.
   - The supervisor has no GitHub access, and no AQ command returns a failed
     check's log; the hold notice only names the check ("fmt, clippy and
     contracts").
4. The real failure was a flaky, unrelated test; a rerun passed.

Separately, `aq conversation project` and `aq conversation global` print the
conversation's discovery record (id, revision, runtime, limits) and no
messages, under the description "A project's supervisor conversation". Reading
it, the conversation looks empty and its `revision` (1) looks like a message
count. That led to a wrong diagnosis that the supervisors never ran.

## 3. Goals

1. A task's attempts each get their own time window; a retry is never refused
   because earlier attempts used time.
2. Every hold, whatever its source (server budget and limit holds, playbook
   `work.hold`, delivery holds, never-granted offers, policy holds), notifies
   the project supervisor once, with what it needs to act.
3. The project supervisor can see why the task is held, including a failed
   check's log, without GitHub access.
4. A hold the project supervisor can't resolve goes to the global supervisor,
   which decides whether the owner is told, and the owner is told outside the
   dashboard.
5. No CLI read misleads: reading a conversation shows its messages.

## 4. Not goals

- Supervisors on Claude. The supervisor service is Codex-only today
  (`aq-supervisor-control/src/api.rs:63,144`, `aq-supervisor-host/src/service/plan.rs:101,168`);
  Claude support is separate work.
- Giving supervisors GitHub credentials.
- Changing who may retry. A project supervisor already holds the owner's
  powers inside its project and is told to `task.retry` once a hold's cause is
  resolved (`aq-playbook-domain/src/delivery_decisions.rs`).

## 5. Design

### 5.1 Each attempt starts the timer over

- Remove the lineage time budgets `max_lineage_sandbox_ms` and
  `max_lineage_elapsed_ms` from Selection defaults, profile `## Selection`
  overrides, the replacement check and the hold reasons
  `task_sandbox_minutes_budget_exhausted` and `lineage_time_budget_exhausted`.
  Clean cut: no compatibility shim; stored Settings drop the keys.
- An attempt is bounded by its execution profile's `wall_time_ms`. Retries are
  bounded by `max_attempts`. Spend stays bounded by `max_cost_microunits`.
- Interim (done 2026-11-22): both budgets raised to 64,800,000 ms (18 h) in
  the live Selection defaults (revision 34).

### 5.2 Every hold notifies the project supervisor

- When a task enters `held` readiness for any reason, the server sends one
  notice to the project's supervisor conversation, once per hold (keyed by
  task, attempt and hold reason). Releasing and re-holding sends a new one.
- A notice carries the task and project ids, the hold reason code and
  message, the attempts with their outcomes, the recorded result and failed
  check details (5.3), and the commands that apply (`task.work`,
  `task.history`, `task.retry`, `attempt.abandon`, `task.edit`,
  `task.cancel`).
- The delivery hold notice that exists today becomes one case of this.
- The server only delivers the notice. What to do is the supervisor's
  decision (policy lives in playbooks and supervisors).

### 5.3 The supervisor can see why a check failed

- When delivery records a failed required check, it also records the failing
  job's log tail through the GitHub App: the last `check_log_bytes` of each
  failed job, redacted with the credential scanner's patterns. It is retained
  with the result and rolled like other result records.
- `task.work` (and the hold notice) include that tail. Project supervisors
  can read it for their project.
- `check_log_bytes` is a Setting.

### 5.4 Escalation: project, then global, then the owner

- A project supervisor that can't resolve a hold sends it to the global
  supervisor with a new command `supervisor.escalate {task_id, summary}`. The
  global conversation receives it as mail with a link back to the project
  conversation.
- The global supervisor decides whether the owner must act. If so it asks the
  owner with the existing `question.ask` (a question with the task id and the
  decision needed). Grant `question.ask` to the global supervisor if it lacks
  it; project supervisors keep escalating through the global one.
- Owner questions and notifications already go through the existing
  notification delivery, which has a fixed Discord v10 webhook adapter
  (`aq-notifications/src/discord.rs`) configured as endpoints with a protected
  secret file (`aq-server/src/notifications/config.rs`). The live stack has
  no endpoint, hence `no_external_route`. Configure one (an operations step,
  needs the owner's webhook), and make the global supervisor's conversation
  say when a question could not leave AQ.
- Supervisor instructions: the project supervisor resolves holds it can (retry
  after a verified transient failure, file a fix task, edit the task), and
  escalates otherwise; the global supervisor triages escalations.

### 5.5 Remove the misleading conversation reads

- Remove the CLI commands `conversation.project` and `conversation.global`
  from the command registry. The HTTP discovery routes stay; the dashboard
  uses them.
- `aq supervisor list` already gives each conversation's id. Reading
  messages is `conversation.messages`
  (`/api/human/conversations/{conversation}/messages`); give it a typed CLI,
  `aq conversation messages --conversation-id <id> [--limit N]`.
- Update `skills/aq-comms/SKILL.md` and the agent-commands test.

### 5.6 Diagnosability

- The server's "API request refused" log line includes the method and route
  template (not the full path or query, which can carry ids), so a 404 like
  the 15:39 ones can be traced.
- Diagnose the 503 overload of 07:00–09:00Z on 2026-11-22 (1,393 refusals)
  and the `409 native runtime qualification unavailable` refusal that repeats
  every ~10 s (6,186 in 26 h). Both are separate tasks once their cause is
  known.

## 6. Order of work

1. aq-server: 5.5 (remove the misleading reads, typed `conversation messages`)
   and 5.6's refusal-log route.
2. aq-server: 5.1 (remove lineage time budgets).
3. aq-server: 5.2 (hold notices for every hold).
4. aq-server + aq-git delivery: 5.3 (failed check log tails).
5. aq-server: 5.4 (`supervisor.escalate`, `question.ask` for the global
   supervisor, instructions); operations: configure the Discord endpoint.
6. Diagnose the overload and the 409 loop (5.6).

## 7. Questions for the owner

1. The Discord webhook for owner questions and notifications (5.4): which
   channel? Put the webhook URL in a protected file on the host and give me
   the path (never paste it into chat).
