# 0012: Internal work (one way for AQ components to use a model)

**Status:** Draft for the owner's approval (2026-11-12), from a brainstorming session with the owner; a builder review's questions are answered in the text. Nothing here is implemented.
**Repositories:** aq-server (service, storage, supervisor host), aq-dashboard (Settings, History, inline run view).
**Needed by:** spec [0011](0011-routing-policy.md), for its classifier and its policy compiler.

Decisions are marked **(owner)** when the owner made them and **(author's call)** when the author filled a gap. The owner should check the second kind while reading.

## 1. Why

Several parts of AQ need a model to do a piece of work that is not a project task:

- classifying a task for routing;
- compiling a plain-language playbook or policy source into its structured form;
- calls a playbook makes for a judgment;
- revising a decision from the owner's feedback;
- processing a result for the Results page.

Today there is one implementation, unmerged: aq-server #64 sends a single request to an API endpoint with a key read from a file, wired to playbook judgments only. The owner's providers on the live stack are subscription logins, not API keys. Without a shared mechanism each of the five callers would grow its own way of reaching a model, its own configuration and its own failure behavior.

**(owner)** There is one mechanism. Any AQ component uses it, and it can be configured to serve a request through an API call or through one of the logged-in agents. The caller does not know which.

## 2. Goals, non-goals, anti-goals

**Goals**

- One service, one request shape, one place to configure it.
- A request is never queued behind project work and never takes capacity from it **(owner)**.
- When the login a request would use is at its usage limit, it falls back to another provider **(owner)**.
- Failures are handled without the owner wherever possible **(owner)**.
- The routing classifier and the policy compiler can be built on it first.

**Non-goals**

- **Restricting what internal work may do.** **(owner)** No lockdown scheme for now, until it is clearer what internal work is needed for. Internal work gets what the server's agents already have and nothing is added to confine it further.
- Internal work that reads a repository. The design leaves room for it (§4); the first version does not build it. Decision revision and result processing wait for it.
- Changing how project tasks, supervisors or manual agents use models.

**Anti-goals** (this would be a failure even if it works)

- A second or third way to call a model appearing elsewhere in the codebase. New model use by an AQ component goes through this service.
- Internal work stuck behind project tasks, a full worker, or a provider's agent limit.
- A task left waiting on internal work with no visible reason.

## 3. What a caller asks for

A component makes a request with:

| Part | Meaning |
| --- | --- |
| Purpose | What the work is for, from a fixed list AQ defines **(author's call)**: routing classification, playbook compilation, playbook call, decision revision, result processing. Free text is not accepted, so a typo cannot silently fall through to the default. |
| Instructions and input | Everything the model needs. In the first version the model is given nothing else to look at. |
| Answer shape | The structure the answer must have. An answer that does not fit is a failed run, not something the caller has to check. |
| Urgency | Whether something is blocked on the answer. |
| Affected item | What the work is about: a task, a policy, a playbook run, a decision, a result. Used for display and for routing failures (§7, §8). |
| Limits | A deadline and a size limit for the answer. |

The caller gets back the answer, or a typed failure saying what went wrong and what was tried. It also gets a record identity it can show (§7).

A caller may ask and wait, or ask and be told later; the routing playbook's judgment step is the second kind.

## 4. Internal tasks

**(owner)** Each request runs as an internal task: a task tagged internal, belonging to no project, hidden from the dashboard by default.

- **It runs on the server.** **(owner)** It takes no worker slot.
- **It is outside project capacity.** **(owner)** It is not ordered with project tasks, does not count against the per-provider agent limits or the swarm concurrency limits in Settings, and is not stopped by a full queue. The supervisor host today reserves a provider and profile cell for each turn; internal tasks do not.
- **Among themselves** **(author's call)**, internal tasks that something is blocked on run before those nothing is waiting for, and otherwise oldest first.
- **Workspace.** **(owner)** Internal tasks can have different workspace needs. The first version has one kind: no workspace at all. A later kind may be given a repository to read; the task record carries what kind it is from the start so that adding the second does not change the first.
- **Where the record lives.** **(author's call)** An internal task is a record kept by the server, not a task in the graph. The graph requires every task to have a project, and putting internal tasks there would mean filtering them out of every scheduler, count, view and retention path. The dashboard still presents it as a task. Supervisors reach internal tasks through their own read and retry surface (§8), since their graph access does not cover them.
- **Lifetime.** An internal task is short. It ends with an answer or a failure and is kept as history (§7).

**(owner)** The swarm availability switch and reduced-usage mode do not stop internal work. The owner may later want a stop and start for the whole of AQ; that is not part of this spec, but if it is built, internal work stops with it.

## 5. Choosing the model

**(owner)** Settings has one default profile for internal work, and optional overrides.

- The default applies to every purpose that has no override.
- An override points one purpose at a different intelligence profile. The playbook compiler, for example, may need a stronger one.
- **(author's call)** A playbook call may ask for a stronger or weaker level than the playbook-call default, within what the owner allows.
- **(author's call)** This is on Settings > Intelligence, beside the profiles and the routing policy.

A profile is already an ordered list of models across providers. That order is the fallback: when the first provider's login is unavailable or at its usage limit, the next entry takes the request **(owner: fall back to another provider)**. No separate fallback rule is added.

## 6. Two ways to run it

An entry in a profile is served one of two ways. Which one is a property of the provider's configuration, not of the request.

- **API.** One request to a provider endpoint with a key. #64's request code is the starting point, moved out of the playbook module into the shared service.
- **Logged-in agent.** The work is given to a harness that is logged in on the server, run once and non-interactively: prompt in, answer out, exit. The supervisor host is the base. It already runs logged-in agents on the server with no worker identity, and its subscription-backed turn already invokes a harness non-interactively. What it lacks is a run that is not a conversation turn: no mailbox, no reply credential, no retained session to re-adopt.

**(author's call)** A task with no workspace runs with the harness's tools off: one completion, no tool loop. This is what "given nothing else to look at" means in §3, and it keeps the promise the playbook judgment command already makes to playbooks. It is a property of the no-workspace kind, not a restriction scheme; the later repository-reading kind runs with tools.

Facts from the code that the build starts from:

- Whether a provider is served by API or by login is already recorded on its credential (`aq-selection`, `CredentialKind`: `ApiKey` or `SubscriptionLogin`).
- **(owner)** Both Codex and Claude logins are served on the server from the first logged-in version, so that fallback between providers is real. Both are already supported subscription harnesses on workers.
- The supervisor host's subscription turn runs Codex only (`codex exec --ephemeral --output-last-message`, ChatGPT login). The server holds Claude logins but only hands them to workers; nothing runs Claude on the server today, so a non-interactive Claude run in the server's agent container is new work in I2.
- Usage-limit signals exist for OpenAI and Anthropic logins (`aq-provider-quota`).
- `judgment.request` already completes later with an event, and its failures are already typed (`provider_error`, `provider_unavailable`, `deadline_exceeded`, `budget_exceeded`, `invalid_output`). Those become the service's failure types.

Both produce the same record: what was asked, which profile, model and provider answered, by which of the two ways, how long it took, what it cost in usage where known, and the answer or the error.

The existing playbook `judgment.request` command is served by this service, with purpose "playbook call". The direct provider binding #64 proposed is not added.

## 7. Seeing it

- **Hidden by default.** **(owner)** Internal tasks do not appear in Work, Overview counts, or project views.
- **Shown where it mattered.** **(owner agreed)** The item an internal run was about shows the run inline: what was asked, which model answered, how long it took, the answer or the error, and a retry. A task shows its routing classification; the routing policy shows its last compile; a decision shows its revision.
- **Browsable.** **(owner agreed)** Agents > History lists internal runs behind an "internal" filter that is off by default. History is the manual-agents archive today, so this adds a second source to it.
- "Needs your attention" items are computed when the page is read; internal-work failures are a new kind there.

## 8. When it fails

**(owner)** Failures should not normally reach the owner. Three steps:

1. **The run handles it.** The profile's next provider is tried. If every entry fails, the caller's own safe default applies where it has one. The routing policy's is "assume medium risk and route anyway".
2. **A supervisor gets it.** **(owner)** If the work is still stuck, or the same purpose keeps failing, a supervisor is told what failed and why. It may retry, or fix the cause first and then retry: a profile with no working model, a source that will not compile.

   **(owner)** Supervisors are super-user operators, able to do and fix almost anything. Today a supervisor's credential covers only graph reads and writes, enqueueing, messaging and decisions, and it cannot save profiles or the routing policy. The owner calls that a misunderstanding by an earlier agent: too much security too early. This spec therefore includes giving supervisors the authority to change Intelligence settings (profiles, the internal-work default and overrides, the routing policy) and to see and retry internal tasks. Widening supervisor authority beyond what internal work needs is the same correction and should follow, but is outside this spec.
3. **The owner gets it.** **(owner)** Only when the supervisor cannot fix it. The item appears in "Needs your attention" with the supervisor's diagnosis, and it opens the affected item, not the internal task **(owner agreed)**.

Details **(author's call)**:

- The project's supervisor handles a failure that belongs to a project's task. The global supervisor handles the rest.
- A supervisor has a fixed number of attempts at one failure before it must escalate.
- The supervisor's own model use is its ordinary session, not an internal task, so a fault in internal work cannot disable what repairs it.
- A single failed run that the caller's default absorbed raises nothing. It is visible inline and in History only.

## 9. Order of work

| Phase | Delivers |
| --- | --- |
| I1 | The service, the internal task record, the request and answer shapes, and the API way of running it. Settings default and overrides. `judgment.request` served through it. |
| I2 | The logged-in agent way for Codex and Claude, on the supervisor host, with fallback across a profile's providers. |
| I3 | Inline run view, History filter, supervisor authority over Intelligence settings and internal tasks, and the failure path through a supervisor to "Needs your attention". |
| Later | Internal tasks with a repository to read; decision revision and result processing. |

Spec 0011's classifier can start on I1 or I2, whichever the owner's providers allow first. With subscription logins only, that is I2.

## 10. Usage

**(owner)** Internal work uses the same accounts as project work, so its usage is shared with it: there is no separate budget or limit. Each run records the usage it cost, where the provider reports it, so internal work can be reported as its own line in metrics.

## 11. Open questions for the owner

None.
