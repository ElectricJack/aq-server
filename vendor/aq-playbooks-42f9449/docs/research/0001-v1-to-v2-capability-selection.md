# Selecting a small v2 playbook command surface

**Status:** Direction accepted and incorporated into the design; supporting audit, not an API specification  
**Date:** 2026-10-30

The recommendation is an initial surface of **twelve concrete host commands**, plus native workflow operations. Existing command names are evidence of needs, not a migration checklist. Application-specific integrations remain optional registrations; they do not enlarge the core engine.

The audit inventoried the Python AQ application at checkout `9a9b8e8ef`, its shipped policy sources/bundles, and the local vault's canonical sources and retained bundles. That application internally calls its current playbook artifact format “V2”; here, v1 means the existing Python application, and v2 means the new Rust ecosystem.

The offline catalog contains 586 discoverable entries: 582 registrations and four explicit denials. The registrations comprise 245 typed contracts and 337 generated JSON-object adapters. Many entries are administration, presentation, migration, or execution bookkeeping. Catalog discovery is not execution authorization. The daemon was unreachable during the audit, so current activation, loaded external plugins, and live command availability remain unknown.

Across the primary repository/vault sources there are 25 distinct policy identities. Primary compiled bundles reference 61 distinct command names directly and five through declared model tools, with overlap giving 65 distinct names. Six of those names are absent from the current catalog. An additional behavior surface exists in declarative integration-policy blocks and built-in agent-task steps. Neither 582 nor 65 is the proposed v2 command count.

## 1. Proposed initial commands

| Command | Narrow contract | Existing needs it absorbs |
| --- | --- | --- |
| `context.read` | Read one of a finite set of typed, scoped views: work/project context, execution options, evidence, or health observations. Return version/freshness/completeness and bounded snapshot pages. | Task/project reads, downstream context, provider availability, CI baseline observations, diagnostic scans |
| `work.ensure` | Idempotently create a bounded work item or proposed work graph from versioned inputs, acceptance conditions, dependencies, and execution constraints. Return durable work/operation references. | Task creation, investigation, review, spec ingestion, fidelity review, repair, project setup, supervisor report requests |
| `work.change` | Apply a checked change to requirements, priority, dependencies, execution constraints, or individually owned holds. Expected revisions and source/evidence references are required where applicable. | Editing work, adding dependencies, holding/releasing batches, routing preferences, revising a plan |
| `judgment.request` | Request a bounded assessment against explicit context and a declared result schema; return an asynchronous operation handle. Host-selected model/profile, with no arbitrary tool loop in the baseline. | Routing classification, risk assessment, interpreting uncertain findings without creating a worker task for every short judgment |
| `decision.request` | Ask eligible decision makers a typed question tied to an exact subject/revision, with explicit expiry. The host owns authenticated answers. | Human gates, exceptions, escalation choices, proposal acceptance |
| `check.request` | Run a registered check against an immutable subject and named execution context; produce typed outcomes and retained evidence. | Candidate CI, regression checks, compatibility checks, deterministic project-health checks |
| `candidate.prepare` | Prepare an immutable integration candidate from an explicit bounded source set and base; return candidate identity or structured conflict/refusal. | Frozen integration batches, candidate construction, collecting prepared changes |
| `candidate.publish` | Request conditional publication of a candidate using its evidence and destination preconditions. The integration host enforces current authority and delivery invariants. | Publication, exact-candidate validation, safe retries and destination-moved handling |
| `message.send` | Submit a scoped notification or report using a stable identity and declared visibility. Acceptance, delivery and acknowledgment remain distinct. | Notifications, supervisor messages, report delivery |
| `findings.reconcile` | Reconcile a bounded, scoped observation partition with durable finding episodes; atomically update episode state and outgoing notification intents. | Stuck-work alerts, provider-health alerts, project-health findings, suppression, reminders, and cleared notifications |
| `operation.get` | Read the authoritative status/result of a previously admitted asynchronous command, preserving its operation identity and outcome vocabulary. | Job/result reads, completion polling, recovery after a lost response |
| `operation.cancel` | Request cancellation through the owning adapter and return accepted/refused/too-late/confirmed/indeterminate status as applicable. | Canceling requested work, checks, judgments, pending questions, and cancellable integration operations |

This is a proposed semantic surface. The host adapter owns the service implementations; every listed command must have a bounded schema and tested contract before it is exposed. The table does not imply these APIs exist today.

`work.ensure` has a declared kind such as implementation, investigation, review, planning, or repair, plus an output contract. Its kind selects a permitted work template, not an arbitrary server method. Reviews return findings and recommendations; completing a review cannot impersonate an authorized decision. Work results and check evidence are recorded by their owning services, so the initial playbook surface does not also need a general artifact-write command.

`work.change` cannot patch arbitrary fields or set any status to any value. Cancellation uses `operation.cancel`; acceptance and delivery come from domain-owned outcomes. Routing changes express permitted requirements/preferences, while AQ still selects and authorizes an actual execution using live capacity and worker eligibility. A policy cannot choose an arbitrary worker credential or bypass assignment fences.

`context.read` is not SQL or a proxy to hundreds of hidden endpoints. Each supported view has a published schema and scope. New view types and new `work.change` variants count as API growth and must be justified by a workflow, even if no top-level command is added.

`operation.cancel` is one lifecycle interface over registered adapters, not a universal process killer. A prepared candidate may have no active operation to cancel. An already published change is not undone by cancellation. Each adapter declares its actual cancellation and reconciliation guarantees.

## 2. Generalization that removes existing commands

The highest-value reduction is moving reusable coordination out of domain-specific daemon verbs and into composed workflows:

| Existing shape | Proposed composition |
| --- | --- |
| `ensure_spec_ingest`, `ensure_fidelity_review`, several task creation verbs | `work.ensure` with a declared role/output schema and a source-bound decision or evidence reference |
| `task_batch_commit`, `task_batch_release`, dependency/routing gates | Validated graph admission through `work.ensure`, followed by authorized owned-hold changes through `work.change` |
| `task_failure_triage_notify`, `task_recovery_notify`, bespoke escalation creation | Read failure context, ensure investigation when needed, request a decision when needed, and send an identified notification |
| `morning_report_tick`, `digest_request`, `report_reconcile` | A report sub-workflow: capture context, request supervisor work, wait until deadline, choose authored or deterministic fallback, send once |
| Provider probing/rerouting/notification combinations | Registered health checks or observations, typed assessment where needed, permitted work constraints, and finding reconciliation |
| `playbook_state_get/set/list/notify` for monitoring | One findings service that owns episode revisions, reminders, notification intents, and recovery |
| Numerous integration preparation/verification/repair verbs | Candidate prepare, named checks, repair work, candidate publish, and operation recovery; mechanics stay inside the integration service |
| `run_playbook`, `resume_playbook`, command-level child orchestration | Native call, owned async start/join, independent start, and durable wait/return operations |

A command's smaller name count is not sufficient justification for combining it. Candidate preparation and publication remain separate because their effects, authority, and outcomes differ. Checks and judgments remain separate because a model's assessment cannot substitute for executed test evidence. Human answers remain separate from agent-produced findings.

These compositions still require domain guarantees. For example, a generic work request cannot accept a caller's claim that a spec was approved: AQ must validate the cited decision and source revision. A report workflow cannot broaden visibility when handing context to a supervisor. A notification cannot be considered delivered merely because a message was accepted.

The report composition is an intended replacement, not an immediate equivalent of every reporting API or dashboard feature. To replace a production reporter it must demonstrate frozen context, single author request, visibility restrictions, deadline behavior, deterministic fallback, and duplicate-safe delivery. Until then, defer that workflow or retain its existing domain adapter behind an optional registration.

## 3. A small findings capability instead of general policy state

The two stuck-task watchers make a concrete case for durable state, but most of their state is specifically about finding episodes. A general key/value API in the runner would be a larger commitment than their immediate requirements justify.

`findings.reconcile` takes a partition identity, observation/evaluation references, bounded finding identities and evidence, and a declared reminder policy. The host derives scope and sender. It owns episode transitions and a durable message outbox; AQ messaging consumes those intents idempotently. This preserves atomic intent without coupling the playbook store to message tables.

Clearing requires authoritative complete coverage for the partition and the expected prior revision from before the scan. Incomplete or expired scans can report positive findings but cannot prove an existing finding disappeared. A caller-supplied `complete: true` alone is insufficient; the host verifies the referenced observation/evaluation records and applicability. Changed revisions refuse a stale clear instead of retrying old absence evidence against newly read state.

This supports present/cleared episodes, unchanged suppression, reminders, scoped visibility, and restart recovery without exposing arbitrary durable policy state. Arbitrary cross-run state can remain deferred until another use case requires it.

## 4. Policy-by-policy selection

This covers every distinct identity in primary repository sources, primary vault sources, and non-archival bundles. Multiple retained versions and project/system copies are listed in the machine inventory; their presence does not establish activation.

| Existing policy | V2 disposition |
| --- | --- |
| `default-assignment-routing` | Preserve routing policy. Use context, bounded judgment, and checked work constraints; AQ owns scheduling/admission. |
| `provider-usage-probe` | Preserve availability observations through registered probes. Avoid requiring an agent turn for a deterministic probe. |
| `provider-failover` | Preserve fallback/hold intent. Use observed options and work constraints; provider lockout remains host mechanism. |
| `default-pipeline` | Preserve approved-input provenance, graph validation, and conditional release. Compose ingestion/review/decision sub-workflows rather than porting special verbs. |
| `supervisor-failure-triage` | Preserve one incident/notification per failure episode through work keys and finding episodes. |
| `blocked-task-escalation` | Fold into the shared failure/blocked-work workflow, retaining the distinction between execution failure and dependency blocking. |
| `ci-main-sentinel` | Preserve baseline observation, bounded repair, and ownership of failing tests. Use checks/evidence, repair work, and decisions. |
| `stuck-task-watch` | Preserve with complete observation snapshots plus `findings.reconcile`. This is a useful pilot for monitoring. |
| `stuck-task-watch-reminder` | Make reminder cadence a parameter of the same watcher; no duplicated graph needed. |
| `morning-report` | Reauthor as a report sub-workflow with deadline/fallback; retain host-owned visibility and duplicate-safe delivery. |
| `supervisor-digest` | Share the report workflow with different trigger/window/recipient parameters. |
| `supervisor-hourly-report` | Share report coordination; a supervisor request is distinct from launching a worker. |
| `project-defaults` | Preserve idempotent onboarding work. Replace hard-coded local helper paths with declared project/worker capabilities. |
| `github-issue-triage` | Optional GitHub adapter supplies issue observations and authorized external writes; reuse investigation/review/decision workflows. |
| `fast-smooth-train` | Preserve simple merge/wait/repair policy and exact-evidence publication. Treat its declarative settings separately from the trivial observer graph. |
| `parent-integration` | Reauthor useful dependency/evidence behavior; do not carry the old hierarchy-specific command chain wholesale. |
| `root-train` | Reauthor around immutable candidates, checks, repair work, and publication. Legacy operation names are not compatibility requirements. |
| `agent-queue-parent-integration` | Express project differences as parameters or composition over the shared integration workflow. |
| `agent-queue-root-train` | Express project differences as parameters over candidate integration; avoid another copy of the engine's policy. |
| `promotion-request` | Optional promotion/release workflow; reuse decisions and operation handles. Domain promotion/backmerge mechanics stay in an adapter. |
| `promotion-continuous` | Share promotion workflow with an explicit request/supersession policy parameter. |
| `pr-merge-sweep` | Do not port its automatic merge instructions. Preserve a reconciliation check for stranded delivery through the new integration owner. |
| `object-loop` | Optional Matter adapter. Preserve pinned evidence, shared budgets, checkpoints, and quality criteria; reuse bounded calls/joins/retries. |
| `quilt-health-watch` | Optional registered project-health check plus finding episodes. Remove UTC substring calendar approximations and unnecessary agent wrappers for mechanical checks. |
| `memory-consolidation` | Reauthor useful knowledge maintenance against aq-graph. Retire filesystem memory helpers and the old model-driven tool loop. |

## 5. What stays out of the initial surface

The first release does not expose the full profile/configuration/plugin API, dashboard layout, daemon lifecycle, worker leases, pool CRUD, workspace repair, database migration, legacy cutover, or raw Git mutation verbs to ordinary playbooks. Those are host administration or execution mechanisms with their own interfaces.

Discovery may describe unavailable or privileged capabilities, but discovery grants nothing. The four current explicit denials—`run_command`, `shutdown`, `restart_daemon`, and `update_and_restart`—do not become generic workflow escape hatches.

Optional registrations can add managed scripts/jobs, GitHub writes, deployment, or domain experiments when a project needs them. Managed scripts have pinned inputs, bounded output/time, scoped credentials, and operation recovery. They are not a synonym for unrestricted shell execution. Versioned knowledge can be exposed through aq-graph-backed capabilities when the knowledge workflow is implemented.

Do not automatically register every daemon handler as a playbook command. V1's generated adapters make a large operational API discoverable, including opaque or administratively scoped entries. V2 should register a deliberate policy API with published outcomes and conformance tests. Keep administration available through AQ without making it part of the default policy catalog.

## 6. Changes suggested for the first-pass design

1. Replace the nine broad families as the proposed initial host API with the twelve concrete contracts above. Keep extensibility in the registry rather than in an unrestricted operation argument.
2. Add `judgment.request` explicitly. Routing already uses a short typed model assessment; it should not require a fully scheduled worker assignment or embed an LLM SDK in the runner.
3. Add a findings/observation contract early if monitoring is part of the pilot. General engine-owned cross-run state can remain deferred.
4. Define complete finite snapshot traversal and bounded collection operations such as filter, map, membership, and set difference. Nested iteration is already exercised by real monitoring policies. Large results remain references/pages rather than parent snapshots.
5. Support bounded reconciliation workflows. Object-loop, provider recovery, and reporting use events as wake hints and reread durable state. A runner must be able to revisit until settled with a deadline/backoff, without creating one endless chain of child workflows.
6. Define host-consumed policy data separately from workflow execution. `fast-smooth-train` has a terminal-only graph and a meaningful declarative train policy. Prefer a versioned host-owned configuration artifact referenced by workflow inputs; a pure policy-evaluation interface is an alternative to evaluate later. Do not hard-code train fields into the generic playbook model.
7. Specify basic timezone/calendar occurrence and restart misfire behavior through a host scheduler adapter. Existing monitoring policies contain UTC-string workarounds that should not survive. Complex calendars need not become engine expressions.
8. Keep sub-workflow reuse central: parameterize watcher reminder variants, report cadence variants, and project integration overrides instead of duplicating their graphs.

These recommendations have been incorporated into the [design](../specs/0001-aq-playbooks.md) at the user's request. The design is authoritative for the selected scope; the audit remains evidence rather than a full v1 migration requirement.

## 7. Minimum pilot and expansion test

The pilot should cover four workflows: coding/integration with bounded repair; routing with a typed assessment; a stateful stuck-work watcher; and one supervisor report with a deadline fallback. Together they test most of the proposed surface and expose different kinds of state, without requiring release automation or a domain-specific renderer.

Before admitting a new command, demonstrate why an existing command plus a reusable sub-workflow cannot express the behavior while preserving authority, atomicity, and recovery. A genuinely new domain effect is a good reason. A new report cadence, reviewer role, task type, or escalation branch usually is not.

Count command variants and view schemas as well as top-level names. A twelve-command API with five hundred hidden operation variants would miss the objective. The initial implementation should publish a finite catalog and add optional capability packs deliberately.

## 8. Inventory and evidence

- [Per-command migration inventory](v1-command-migration.csv): all 586 names, source descriptions, contract class, usage references, and proposed family-level disposition. Family labels are recommendations, not individual behavioral parity certifications.
- [Complete offline command catalog](v1-command-catalog.json): input/output schemas, outcomes, authority/retry metadata, and source links for the offline registry.
- [Playbook inventory](v1-playbook-inventory.json): 144 source/bundle records, including 70 archived/repair records, source digests, executable command references, triggers, and step kinds. Hidden vault worktree copies and Markdown hub pages are excluded.

The primary records comprise 16 repository sources, 17 repository bundles, 11 vault sources, and 30 non-archival vault bundle files representing 21 bundle identities. Their union has 25 distinct policy identities. The compiled primary bundles declare 45 distinct trigger names; canonical `project-defaults` source additionally requires `project.created`.

Six retained bundled command names are missing from the current offline registry: `delivery_promote`, `integration_build_candidate`, `integration_promote_main`, `integration_reconcile_promotion`, `integration_seal`, and `task_route_options`. Some other integration-policy primitive names are consumed by an integration-specific evaluator rather than the ordinary playbook command registry. Raw command-step counts therefore understate policy behavior.

Repository and installed source also differ. For example, the repository morning report uses `report.morning_due` and durable minute waits, while the installed canonical source still uses `timer.1m`; neither source alone proves what artifact is active. The inventory retains version distinctions rather than choosing one silently.

Key inspected sources include the [registry](../../../agent-queue2/src/commands/contracts/registry.py), [descriptor/denial rules](../../../agent-queue2/src/commands/descriptors.py), [default pipeline](../../../agent-queue2/src/prompts/default_playbooks/default-pipeline.md), [stuck-work watcher](../../../agent-queue2/src/prompts/reviewed_playbooks/stuck-task-watch/source.md), [notification transaction](../../../agent-queue2/src/playbooks/state_notify.py), [train policy data](../../../agent-queue2/src/prompts/integration_playbooks/fast-smooth-train.md), and [bounded object workflow](../../../agent-queue2/src/prompts/reviewed_playbooks/object-loop/source.md).

This is a static capability/selection audit. It does not claim current deployment activation, measured runtime performance, or that the twelve proposed commands are implemented.
