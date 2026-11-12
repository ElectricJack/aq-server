# 0011: Routing policy (the routing playbook, the route on a task, and placement that honors it)

**Status:** Approved by the owner on 2026-11-12. Nothing here is implemented except the Settings field in §2.
**Repositories:** aq-server, aq-playbooks (host catalog), aq-dashboard.
**Depends on:** spec [0012](0012-internal-work.md) for every model call, and aq-server #131 and aq-dashboard #105 (the Intelligence structure setting, migration 117). **(owner)** The work is stacked on those two pull requests, on branch `feat/internal-work` in each repository, without waiting for them to merge.

Decisions are marked **(owner)** when the owner made them and **(author's call)** when the author of this spec filled a gap. The owner should check the second kind while reading. Open questions are in §10.

## 1. Problem

The design routes work in two layers (aq-playbooks spec 0001 §6 and §15, requirement P17):

1. A **routing policy**, run as a playbook, reads the permitted execution options, asks for a bounded judgment only when a fact is missing, and changes the task's authorized requirements and preferences.
2. **Placement**, owned by the server, chooses and authorizes the actual execution from provider availability, capacity and worker eligibility.

Only the second layer runs. As audited on 2026-11-12:

- Placement resolves a model from the project binding alone. `compose::compile` requires exactly one agent profile per binding, and the resolver's request carries no task-level field. Every live project is bound to the automatic router, whose one qualified candidate is a single Claude model, so every task runs on it.
- The routing pilot (`playbooks/coding/pilots.rs`) passes its tests but cannot route. It reads `execution_options` for an attempt that already exists, so it runs after placement. It writes free text into `aq:execution_constraints`, which nothing reads. Nothing starts it: no route exports the artifact and no event triggers it.
- `judgment.request` has no provider bound and refuses.
- The task fields a human can set (`aq:task_type`, `aq:intelligence_hint`, `aq:profile_hint`) are stored and never read.

v1's lesson W6 applies: its routing runs failed silently and tasks sat READY with no visible reason.

## 2. The policy document

**(owner)** The routing policy is Markdown in Settings > Intelligence, carried over from v1's `default-assignment-routing` playbook.

**(owner, 2026-11-12)** It is written the way an engineer would describe routing to another engineer. It contains no AQ or playbook identifiers, command names, field names, policy blocks or answer schemas. Working those out is the compiler's job (§6).

It is stored as `routing_markdown` in the Intelligence structure document (128 KiB limit, revisioned, owner-only save). On `aq-ms1` the plain-language version was saved on 2026-11-12 and is at structure revision 7 after the owner's own edits; a copy is in [`0011-routing-policy-default.md`](0011-routing-policy-default.md). It says, in prose: what level each kind of work needs, how risk raises that level, who may take work at each risk, and what to do when routing cannot decide. **(owner, 2026-11-12)** It says nothing about free hosted models. They are therefore covered by its rule for anything it does not mention: low risk work only, and only where a profile in Settings lists them as a candidate.

Changes in substance from v1 **(author's call)**, for the owner to check:

| v1 | Here | Why |
| --- | --- | --- |
| Three levels: fast, standard, deep | The four Settings parents: Simple, Standard, Complex, Deep | Settings now has four parents. |
| Design, plan and design review at the deepest level | Complex, high effort | The owner's parent descriptions give spec writing and evaluation to Complex and reserve Deep for work needing advanced reasoning. |
| No rule for reaching the deepest level | Deep only for analysis, planning or spec writing that needs advanced reasoning | Same reason. |
| A failed bug fix escalates to the deepest level | Escalates to Complex | Same reason. |
| Load weights, usage factors, tie order and the preference for Codex on ordinary implementation | Removed **(owner removed the Codex preference, 2026-11-12)** | Placement owns load, quota and tie-breaking. |
| A lane for unverified local models | Removed | It pointed at a level with no assigned profile. |
| Providers the policy does not name | Take only low risk work **(owner, 2026-11-12: any available provider may take it)** | The policy names Claude, Codex and local models; everything else, Gemini and the free hosted models included, falls under this one rule. |

One policy is global **(author's call)**; a per-project policy is later work.

## 3. The route on a task

A route is a typed record on the task, written only through the routing command in §5 or by the owner:

| Field | Meaning |
| --- | --- |
| `class` | Parent and effort, for example `standard-high`. |
| `harnesses`, `providers` | Allowlists. Empty means no restriction. |
| `prefer` | Harnesses to try first among those allowed. |
| `lane` | The lane that matched, if any. |
| `provider_intent` | `any` or `pinned` (a lane with `hold: true`). |
| `reasons` | Typed codes such as `class_raised_for_risk`, `hint_clamped`, `escalated_after_failure`, `classification_failed`, each with its inputs. |
| `classification` | The classifier's answer, or absent when none was needed. |
| `policy_digest`, `structure_revision` | What produced the route. |
| `source` | `router` or `owner`. |

A route is replaced, never edited. An owner route is never overwritten by the router.

Where it lives: the current route is one compact value in the task's graph metadata, written with the task's expected version, so a stale router loses to a newer change. Graph metadata is limited to 16 KiB per task, so the route there is capped at 2 KiB and carries no classification text. The full record of every route a task has had, with its classification and reasons, is kept by the server.

## 4. Placement honors the route

- A route applies only where the project binding's intelligence is the automatic router (`automatic_routing: true`). A binding pinned to a named profile keeps that profile: pinning is the owner's override **(author's call)**.
- For a routed task, composition substitutes the profile assigned to the route's parent and effort for the router. Candidate selection then drops candidates whose harness or provider is outside the route's allowlists. Everything else (qualification, quota, capacity, fallback bounds, node eligibility) is unchanged.
- `provider_intent: pinned` turns "no candidate available" into a hold instead of a fallback.
- **Unrouted tasks wait** **(author's call)**. A ready task under an automatic-router binding with no route is not offered, and the task shows "Waiting for routing" with how long it has waited. There is no silent fallback to the router's default model. This is not one of the existing holds: those record a refusal about a specific attempt and cannot exist for a task that has never had one, so the waiting state is shown from the task's routing state instead.
- **One switch** **(author's call)**. Routing is on or off for the whole installation, in Settings beside the policy. Off, placement behaves as it does today and ignores a missing route; a route that exists is still honored. It is on only once a policy has compiled.
- W6: a task unrouted for longer than 10 minutes, a routing run that failed, and a routed task that no candidate can serve are each a failure with a typed reason. They follow the failure path of spec 0012 §8: the supervisor first, which may retry or fix the cause, and "Needs your attention" only when it cannot. The item opens the task.

## 5. The deterministic planner

A new crate, `aq-routing`, ports v1's planner. It has no I/O.

- Input: the compiled policy (§6), the task's kind, class hint, origin and failed-attempt count, the permitted options (the classes with an assigned profile, and each profile's harnesses and providers as currently qualified), and optionally a classification.
- Output: `planned` with a route; `needs_classification` with the allowed kinds and classes; or `no_candidates` with the reason. Every task is classified once, because the risk answer is needed for almost all of them. Whether a task is already routed is checked by the host command, and waiting for an unavailable provider is placement's job, so neither is a planner outcome.

Two host commands expose it. They need host catalog 2.2.0 in aq-playbooks and a new host profile in aq-server:

- `context.read` view `routing_plan`, keyed by task. It runs the planner and returns its output. It replaces the pilot's attempt-keyed `execution_options` read.
- `work.change` variant `route`, which takes a plan, re-runs the planner against the task's current revision, and writes the route. It returns `routed` or `stale`.

The classification is a structured answer, and the engine cannot read a judgment's answer out of its completion event (the event's output is a union; this is the D11 limitation). So the catalog gains a named answer shape for `judgment.request`, "routing classification", and the second `routing_plan` read names the judgment by its operation. The server looks the answer up; it never passes through the playbook's own data.

The count of failed attempts used for escalation is the number of the task's earlier attempts that failed and were charged. Attempts ended by an owner or service stop, or lost to a reboot, are already exempt and do not count.

## 6. The routing playbook

Rule `route-task`, as in v1:

1. `context.read routing_plan`. `planned` goes to step 4. `needs_classification` goes to step 2. `held` and `already_routed` end the run.
2. `judgment.request` with the compiled classifier instructions and the plan's task fields, limited to one call with no tools. Failure goes to step 3 with `{"failed": true}`.
3. `context.read routing_plan` with the classification.
4. `work.change route`.

**Generation (owner, 2026-11-12).** The playbook is compiled from the policy document. When the Markdown is saved, the authoring agent reads it as the source and produces two things: the structured policy the planner in §5 consumes (levels per kind of work, risk floors, who may take what, escalation), and the classifier's instructions and answer shape. The deterministic compiler then validates and packages them, as aq-playbooks spec 0001 §4 describes. The owner never writes those structures. The compiled result is shown back to the owner in plain terms when it takes effect (for example "bug fixes: Standard high, then Complex after one failure"), and a source the compiler cannot resolve, such as a level with no assigned profile or a provider that does not exist, is refused with the sentence that caused it. The digest on each route is the digest of the compiled policy, which records the source revision it came from. **(owner, 2026-11-12)** A policy that compiles cleanly takes effect immediately, with no approval step. A policy that does not compile changes nothing: the previous compiled policy keeps routing.

**Trigger.** A new host event, `aq.work.route_needed`, emitted by the server when a ready task under an automatic-router binding has no route, and re-emitted at most every two minutes while that stays true. Retry and reopen clear the route and emit it again. The existing `aq.work.changed` is not used: the playbook's own write would trigger it. On the current host profiles the server only emits events as the effect of a playbook command, so this is the first event the server originates there, and starting a run from an event has only been proven on the monitoring profile. Both are new ground for this work.

## 7. Model calls

**(owner)** Routing makes two kinds of model call, and both go through internal work (spec 0012). Routing has no provider, credential or fallback logic of its own.

- **Compiling the policy** is internal work with the purpose "playbook compilation". It runs when the Markdown is saved. Its input is the source text and what Settings currently offers (the levels with an assigned profile, the harnesses and the providers). Its answer is the structured policy and the classifier instructions.
- **Classifying a task** is internal work with the purpose "routing classification". The routing playbook reaches it through `judgment.request`; the routing playbook is built by the server and is the one playbook whose judgment step carries that purpose instead of "playbook call".

**(owner)** Both run on a subscription login. Which profile each uses is the internal-work default or its override in Settings; the catalog's `routing-sol-low` profile is the natural override for classification.

The planner in §5 stays deterministic: a model produces the compiled policy once per save and a classification per task, and never the route itself.

## 8. Dashboard

- The routing policy editor shows the compiled summary, any sentence the compiler could not resolve, and when the current policy took effect. Its caption no longer says execution is connected later.
- The task view shows the route: class, lane, allowlists, reasons and policy digest, or "Waiting for routing".
- The owner can set or clear a route on a task.

## 9. Phases

| Phase | Delivers | Size |
| --- | --- | --- |
| R1 | Route record, owner set and clear, placement substitution and allowlists, the waiting state behind the routing switch. Useful alone: the owner can route by hand. | large |
| R2 | `aq-routing` planner over a compiled policy; compiling the plain-language source into it through internal work, with the summary shown back to the owner. Needs 0012 I1 and I2. | large |
| R3 | Host catalog 2.2.0: `routing_plan` view, `route` variant, the named classification answer shape, `aq.work.route_needed`. New host profile, and the server-originated event. | medium to large, two repositories |
| R4 | Routing playbook built, installed and activated by default, with its classification step served by internal work; the W6 failure path (needs 0012 I3); dashboard §8. | medium |
| R5 | Live acceptance on `aq-ms1`: a low-risk docs task and a high-risk migration task route to different classes; a classifier outage still routes with `classification_failed`; an impossible route holds with an attention item. | small |

**The playbook engine's storage (verified 2026-11-12).** aq-server builds against a vendored copy of the engine at a commit that is not on aq-playbooks `main`; `main` has identical code for those commits plus the bounded-storage change (writer format 3) and the newer catalogs. Catalogs are vendored separately, so a new catalog does not need the engine re-vendored. The risk is growth: the server uses only protected namespaces, bounded storage applies only to unprotected ones, and the D11 pilots saw one tick of eight transitions grow from about 1.3 s to over 5 s inside one namespace, at which point the tick fails. Routing adds a run for every task, so its namespace grows with use.

Phase R3 therefore starts with a measurement: 500 routing runs in one protected namespace, release build, recording tick time. If the last tick is under 1 s, nothing more is needed now. If not, bounded storage for protected namespaces in aq-playbooks, and re-vendoring the engine, become part of this work before live acceptance.

## 10. Open questions for the owner

None. Settled on 2026-11-12: unrouted tasks wait with a visible reason and there is no fallback to the default model; design work routes to Complex; providers the policy does not name take low risk work only.
