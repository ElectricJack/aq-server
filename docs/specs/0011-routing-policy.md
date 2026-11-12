# 0011: Routing policy (the routing playbook, the route on a task, and placement that honors it)

**Status:** Draft for the owner's review (2026-11-12). Nothing here is implemented except the Settings field in §2.
**Repositories:** aq-server, aq-playbooks (host catalog), aq-dashboard.
**Depends on:** aq-server #131 and aq-dashboard #105 (the Intelligence structure setting, migration 117), not yet on `main`.

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

It is stored as `routing_markdown` in the Intelligence structure document (128 KiB limit, revisioned, owner-only save). On `aq-ms1` the plain-language version was saved on 2026-11-12 at structure revision 5; a copy is in [`0011-routing-policy-default.md`](0011-routing-policy-default.md). It says, in prose: what level each kind of work needs, how risk raises that level, who may take work at each risk, how a free model earns wider work, and what to do when routing cannot decide.

Changes in substance from v1 **(author's call)**, for the owner to check:

| v1 | Here | Why |
| --- | --- | --- |
| Three levels: fast, standard, deep | The four Settings parents: Simple, Standard, Complex, Deep | Settings now has four parents. |
| Design, plan and design review at the deepest level | Complex, high effort | The owner's parent descriptions give spec writing and evaluation to Complex and reserve Deep for work needing advanced reasoning. |
| No rule for reaching the deepest level | Deep only for analysis, planning or spec writing that needs advanced reasoning | Same reason. |
| A failed bug fix escalates to the deepest level | Escalates to Complex | Same reason. |
| Load weights, usage factors and tie order | Removed, except the preference for Codex on ordinary implementation | Placement owns load, quota and tie-breaking. |
| A lane for unverified local models | Removed | It pointed at a level with no assigned profile. |
| No Gemini | Gemini takes only low risk work | Gemini has not run live in v2. |

One policy is global **(author's call)**; a per-project policy is later work.

## 3. The route on a task

A route is a typed record on the task, written only through the routing command in §5 or by the owner:

| Field | Meaning |
| --- | --- |
| `class` | Parent and effort, for example `standard-high`. |
| `harnesses`, `providers` | Allowlists. Empty means no restriction. |
| `lane` | The lane that matched, if any. |
| `provider_intent` | `any` or `pinned` (a lane with `hold: true`). |
| `reasons` | Typed codes such as `class_raised_for_risk`, `hint_clamped`, `escalated_after_failure`, `classification_failed`, each with its inputs. |
| `classification` | The classifier's answer, or absent when none was needed. |
| `policy_digest`, `structure_revision` | What produced the route. |
| `source` | `router` or `owner`. |

A route is replaced, never edited, and the task keeps the history. An owner route is never overwritten by the router.

## 4. Placement honors the route

- A route applies only where the project binding's intelligence is the automatic router (`automatic_routing: true`). A binding pinned to a named profile keeps that profile: pinning is the owner's override **(author's call)**.
- For a routed task, composition substitutes the profile assigned to the route's parent and effort for the router. Candidate selection then drops candidates whose harness or provider is outside the route's allowlists. Everything else (qualification, quota, capacity, fallback bounds, node eligibility) is unchanged.
- `provider_intent: pinned` turns "no candidate available" into a hold instead of a fallback.
- **Unrouted tasks are held** **(author's call)**. A ready task under an automatic-router binding with no route is not offered. It shows the hold reason `awaiting_route`. There is no silent fallback to the router's default model.
- W6: a task unrouted for longer than 10 minutes, a routing run that failed, and a routed task that no candidate can serve each raise one attention item naming the typed reason.

## 5. The deterministic planner

A new crate, `aq-routing`, ports v1's planner. It has no I/O.

- Input: the compiled policy (§6), the task's kind, class hint, origin and failed-attempt count, the permitted options (the classes with an assigned profile, and each profile's harnesses and providers as currently qualified), and optionally a classification.
- Output: `planned` with a route; `needs_classification` with the questions, allowed kinds and allowed classes; `held`; `already_routed`; or `no_candidates` with the reason.

Two host commands expose it. They need host catalog 2.2.0 in aq-playbooks and a new host profile in aq-server:

- `context.read` view `routing_plan`, keyed by task. It runs the planner and returns its output. It replaces the pilot's attempt-keyed `execution_options` read.
- `work.change` variant `route`, which takes a plan, re-runs the planner against the task's current revision, and writes the route. It returns `routed` or `stale`.

`judgment.request` completion must carry the answer object, not only its operation id, so the second plan can read it (the D11 limitation).

## 6. The routing playbook

Rule `route-task`, as in v1:

1. `context.read routing_plan`. `planned` goes to step 4. `needs_classification` goes to step 2. `held` and `already_routed` end the run.
2. `judgment.request` with the compiled classifier instructions and the plan's task fields, limited to one call with no tools. Failure goes to step 3 with `{"failed": true}`.
3. `context.read routing_plan` with the classification.
4. `work.change route`.

**Generation (owner, 2026-11-12).** The playbook is compiled from the policy document. When the Markdown is saved, the authoring agent reads it as the source and produces two things: the structured policy the planner in §5 consumes (levels per kind of work, risk floors, who may take what, escalation), and the classifier's instructions and answer shape. The deterministic compiler then validates and packages them, as aq-playbooks spec 0001 §4 describes. The owner never writes those structures. The compiled result is shown back to the owner in plain terms before it takes effect (for example "bug fixes: Standard high, then Complex after one failure"), and a source the compiler cannot resolve, such as a level with no assigned profile or a provider that does not exist, is refused with the sentence that caused it. The digest on each route is the digest of the compiled policy, which records the source revision it came from. **(owner, 2026-11-12)** A policy that compiles cleanly takes effect immediately, with no approval step. A policy that does not compile changes nothing: the previous compiled policy keeps routing.

**Trigger.** A new host event, `aq.work.route_needed`, emitted by the server when a ready task under an automatic-router binding has no route, and re-emitted at most every two minutes while that stays true. Retry and reopen clear the route and emit it again. The existing `aq.work.changed` is not used: the playbook's own write would trigger it.

## 7. The classifier

`judgment.request` needs a provider. aq-server #64 implements one against an API-key endpoint for the older host profile; it is 600 commits behind and fails CI. The `aq-ms1` providers are subscription logins. How the classifier is paid for is the owner's decision (§10 Q2).

## 8. Dashboard

- The routing policy editor shows the compiled summary, any sentence the compiler could not resolve, and when the current policy took effect. Its caption no longer says execution is connected later.
- The task view shows the route: class, lane, allowlists, reasons and policy digest, and the `awaiting_route` hold.
- The owner can set or clear a route on a task.

## 9. Phases

| Phase | Delivers | Size |
| --- | --- | --- |
| R0 | #131 and #105 merged; aq-server `main` CI green. | prerequisite |
| R1 | Route record, owner set and clear, placement substitution and allowlists, `awaiting_route` hold behind a per-project switch. Useful alone: the owner can route by hand. | large |
| R2 | `aq-routing` planner over a compiled policy; compiling the plain-language source into it, with the summary shown back to the owner. | large |
| R3 | Host catalog 2.2.0: `routing_plan` view, `route` variant, judgment answer projection, `aq.work.route_needed`. New host profile. | medium to large, two repositories |
| R4 | Judgment provider bound. | medium |
| R5 | Routing playbook built, installed and activated by default; W6 attention items; dashboard §8. | medium |
| R6 | Live acceptance on `aq-ms1`: a low-risk docs task and a high-risk migration task route to different classes; a classifier outage still routes with `classification_failed`; an impossible route holds with an attention item. | small |

Re-vendoring aq-playbooks to its current `main` is not required for R1 to R6. It becomes required before a long-lived routing namespace grows, because tick cost rises with namespace size and the newer storage format does not yet cover the protected namespaces the server uses.

## 10. Open questions for the owner

1. **Classifier credential.** An API key for a small model (what #64 expects), or a classifier run through an existing subscription login? The catalog has a `routing-sol-low` profile (Codex), which suggests the second; whether the supervisor host can serve a one-shot judgment has not been checked.
2. **Unrouted tasks.** Hold with an attention item (§4), or fall back to the router's default model after a timeout?
3. **The policy changes in §2.** In particular design at `complex-high` instead of a deep class, and Gemini limited to low-risk work.
