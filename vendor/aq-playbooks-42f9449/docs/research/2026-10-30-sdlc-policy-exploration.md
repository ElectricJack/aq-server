# Natural language SDLC policies and playbook capabilities

**Status:** Brainstorm for discussion, not an implementation specification  
**Date:** 2026-10-30

A project should be able to describe its development process in ordinary language, including exceptions, recovery, and what counts as completion. An authoring agent translates that policy into a structured proposal. Deterministic compilation validates and packages the proposal; the runner executes the resulting immutable artifact.

The examples below were developed from first principles by three independent agents and consolidated without consulting existing playbooks. They describe alternative project policies, not one combined policy to activate. The command names are proposed vocabulary, not existing API claims.

The recurring model is **work, evidence, decisions, and external operations**, coordinated by a durable workflow engine. Different SDLC stages usually need different inputs and contracts rather than new engine instructions. The engine can remain small while host capabilities grow.

## 1. What authored policy could look like

Natural language should describe the trigger, subject, required outcome, conditions, exceptions, and what happens when the normal path fails. Authors should not need to spell out JSON paths, run IDs, database transactions, or transport retries.

For example, a small service project might say:

> Start a bug fix when we have a reproducible failure and acceptance criteria. If either is missing, investigate first, for up to one hour. Documentation-only fixes may start immediately.
>
> Ask an independent agent to review behavior changes. Record every blocking finding and either fix it or obtain an accepted rebuttal. Documentation-only changes need link and formatting checks but no separate reviewer.
>
> Build an integration candidate when the change is ready. Run the required checks against that candidate and publish it when they pass. If the candidate changes, obtain evidence that applies to the new candidate before publication.
>
> If a check fails because of a recognized infrastructure problem, retry it once. Otherwise, request a repair. After two unsuccessful repairs, ask the supervisor to choose another approach.
>
> Deploy the published artifact to staging and run smoke checks. Production deployment follows the project's release policy. Record follow-up improvements separately unless they are necessary to satisfy the current acceptance criteria.

The authoring agent must resolve definitions such as “documentation-only,” “independent,” “blocking,” and “required checks” from declared project settings or explicit answers. It should expose unresolved interpretations and generate example traces, rather than silently supply consequential defaults.

Structured frontmatter could identify the project scope, named roles, supported capabilities, and source version. Prose remains the place to explain intent. A model may propose structured definitions for those terms; runtime judgment becomes a separately requested assessment with a typed result.

## 2. Example catalogue

Each example pairs natural language with the main semantic requirement it exposes. Several deliberately require capabilities beyond the first engine release.

### Intake and discovery

1. **“Every request needs a problem statement and an owner before implementation starts. Discovery may start without them.”** Distinguish the prerequisites of implementation from those of investigation.
2. **“An idea can enter the backlog as one sentence. Spend up to thirty minutes investigating before asking me for more detail.”** Timebox a stage and define whether the budget measures elapsed or active execution time.
3. **“If a request duplicates existing work, attach its evidence to that work and tell the requester.”** An assessment identifies the duplicate; an authorized command preserves provenance and scope.
4. **“For ambiguous requirements, choose a reversible interpretation and record it. Ask me if the choice changes a public API.”** Separate an attributed judgment from the deterministic branch that uses it.
5. **“Investigate until we can reproduce the problem and name a plausible fix. After two hours, report what remains uncertain.”** Timebox expiration has its own useful outcome.
6. **“Before planning this experiment, test the two riskiest assumptions. A disproven assumption is a valid result.”** Successful investigation need not lead to implementation.
7. **“When several customers report the same problem, maintain one investigation and attach each report.”** Correlate reports to an explicit problem identity and deduplicate investigation creation.

### Specifications and decisions

8. **“Require a short spec for changes crossing service boundaries. Acceptance criteria on the task are enough for smaller work.”** Conditional obligations depend on a recorded classification.
9. **“Customer-facing behavior needs observable acceptance conditions; refactoring needs a statement of the invariants to preserve.”** Evidence requirements vary by work type.
10. **“Consult the architecture owner before introducing a datastore. Reusing an approved datastore needs no consultation.”** Define the scope and validity of prior decisions.
11. **“For a hard-to-reverse change, compare two viable approaches, including keeping the current system where viable.”** A qualified assessment establishes substance; counting document headings does not.
12. **“I decide product behavior. The implementation team chooses algorithms and libraries within those requirements.”** Decision authority is scoped to a question, not a universal approve button.
13. **“Get agreement from each affected service owner on a shared API. Escalate unresolved choices after two working days.”** Snapshot the required participants, define a join, and supply a business calendar.
14. **“An approval applies to the exact proposal I saw. If its behavior changes, ask again; formatting corrections do not require another decision.”** Bind decisions to revisions and require an authorized assessment for selective reuse.

### Planning and parallel work

15. **“Plan at milestone level. Break a milestone into worker-sized tasks only when it becomes next in line.”** Support progressive elaboration rather than demanding a complete initial graph.
16. **“Start a task when its inputs, acceptance conditions, and blocking dependencies are satisfied. No additional approval is needed.”** Readiness is an explainable domain predicate.
17. **“Have two agents investigate competing explanations independently, then have a third synthesize their findings.”** Fan out work and join identified results; the host manages assignment isolation.
18. **“An implementer may split work while preserving the original outcome and budget.”** The host maintains decomposition, aggregate obligations, and shared budget accounting.
19. **“If work reveals a missing prerequisite, add it and continue anything that does not depend on it.”** Change domain dependencies without freezing unrelated work.
20. **“Open one investigation for a shared blocker even when five tasks report it.”** Use a stable blocker key and an idempotent work request.
21. **“When I change the desired outcome, stop affected assignments and revise the plan. Continue unaffected work.”** Impact assessment, selective supersession, and acknowledged cancellation are distinct steps.

### Implementation and coordination

22. **“Do not start competing implementations unless we explicitly request alternatives or abandon the previous attempt.”** The execution host enforces logical work identity and active assignment ownership.
23. **“If two changes disagree about an interface, hold dependent implementation until the interface is resolved.”** Holds have identities and owners; releasing one hold must not clear another blocker.
24. **“Record useful improvements as follow-ups. Include one now only if it is necessary for the agreed outcome.”** An assessment distinguishes scope expansion from a prerequisite.
25. **“Allow small additional improvements within a shared half-day allowance for this milestone.”** The host reserves and accounts for a cumulative allowance; individual tasks cannot each claim the whole budget.
26. **“After two attempts fail for the same reason, stop automatic retries and ask the supervisor for a different approach.”** Bound business retries using a stable failure identity and persistent counter.
27. **“If a worker disappears, preserve its progress and consider reassignment. Treat a late result from the old attempt as evidence to evaluate.”** Worker leases and replacement are host responsibilities; policy handles the outcome and late evidence.
28. **“Near the research budget limit, produce the best supported recommendation and identify unfinished work.”** Consume authoritative usage thresholds and request a useful bounded finish.

### Testing and evidence

29. **“A bug fix should demonstrate a test failing before the fix and passing after it, or record why automated reproduction is impractical.”** Match evidence to two immutable subjects and represent exceptions explicitly.
30. **“Documentation changes need link and formatting checks. Executable examples must also execute successfully.”** Mixed and uncertain classifications must have defined handling.
31. **“Run targeted checks during implementation and the required suite before integration.”** Check requests name a subject and suite; the host chooses execution mechanics.
32. **“Retry a check once for a recognized infrastructure failure. Investigate a repeated failure.”** Retry eligibility and count are separate from a passing result.
33. **“A quarantined test may fail without blocking delivery if its exception has an owner and an unexpired repair deadline.”** Exceptions are scoped, versioned records with expiry.
34. **“Integration checks must apply to the candidate we actually publish.”** A passing check on the branch head does not prove a later combined candidate.
35. **“If required evidence is missing or stale, request fresh evidence and keep the result unresolved.”** Unknown must remain distinct from pass and fail.

### Reviews and acceptance

36. **“Behavior changes need an independent reviewer. Security-sensitive changes also need a qualified security reviewer.”** Request typed review work with actor constraints enforced by the host.
37. **“Every blocking finding needs a fix, an accepted rebuttal, or an authorized deferral with an owner.”** Aggregate finding dispositions rather than trusting a generic review-completed message.
38. **“Do not repeat review after a spelling correction; renew it when the reviewed behavior changes.”** An assessment establishes what evidence can be carried forward.
39. **“A new dependency needs a justification; an upgrade needs compatibility evidence for supported versions.”** Different predicates can reuse the same work and evidence interfaces.
40. **“If an upstream contract changes, ask affected work owners to reassess their assumptions before accepting their results.”** Invalidate applicability without erasing prior evidence.
41. **“Escalate review disagreement after one rebuttal round, with both positions and their evidence.”** Bound discussion cycles and preserve structured outcomes.
42. **“Notify me of next week's experiment plan. Proceed the next morning unless I object, except for spending money or contacting customers.”** An expressly authorized timeout default needs exclusions, delivery rules, and late-objection behavior.

### Integration and change delivery

43. **“Integrate ready changes independently unless they need validation together.”** The integration host supplies grouping and compatibility facts.
44. **“When the integration baseline breaks, hold new integrations and ask an agent to isolate the cause.”** Apply a scoped admission hold while allowing investigation and preparation.
45. **“After a conflict, ask a repair agent to preserve both changes' intended behavior, then validate the repaired candidate.”** Conflict repair creates new evidence requirements against a new candidate.
46. **“If the destination changes before publication, prepare a new candidate and reassess which evidence remains valid.”** The host enforces conditional publication; policy handles the conflict outcome.
47. **“Canceling a required upstream task should trigger reassessment of its dependents, while preserving independently useful work.”** Cancellation propagation requires domain judgment.
48. **“When the change is delivered, settle its implementation work and publish a summary; leave deferred follow-ups open.”** Delivery, task settlement, and outstanding obligations have separate meanings.
49. **“During an incident, defer routine integrations while allowing investigations and candidate preparation.”** Policy alters eligibility through host-enforced constraints without implementing a scheduler.

### Releases and deployment

50. **“Cut a release every Tuesday from validated changes. Carry everything else into a later release.”** A calendar occurrence selects and freezes membership at an explicit cutoff.
51. **“Ship urgent fixes immediately and include them in the next scheduled release without applying them twice.”** Preserve change identity across release routes.
52. **“Deploy automatically to staging; promote the same artifact after smoke checks pass.”** Promotion preserves artifact identity and uses environment-specific evidence.
53. **“Roll out to five percent, then expand after thirty healthy minutes.”** Define population, complete observation windows, metric thresholds, and freshness.
54. **“If rollout health degrades, return traffic to the previous version and open an investigation.”** Request a host-supported recovery action with a known target and independent follow-up work.
55. **“If monitoring is unavailable, hold the current rollout size and investigate.”** Missing observations must not count as healthy observations.
56. **“Never deploy two versions concurrently to one environment. Reconcile a timed-out deployment before starting another.”** The deployment host owns exclusion and indeterminate-operation recovery.

### Incidents and operational recovery

57. **“Replace a queued deployment with a newer candidate, but let a started deployment finish or recover first.”** Supersession depends on a host-observed commitment boundary.
58. **“Freeze production changes over the holiday window unless the incident commander authorizes a specific exception.”** Calendar constraints and exceptions carry scope, actor, subject, and expiry.
59. **“If a deployment times out, inspect the operation and current environment before deciding whether to retry.”** Timeout is not evidence that the action failed or never ran.
60. **“Watch a release for regressions for twenty-four hours, even after its delivery task closes.”** A monitoring workflow can outlive the work that created it.
61. **“Group alerts for the same outage into one incident and attach later evidence.”** Correlation and event deduplication are different mechanisms.
62. **“Escalate an incident to the backup owner if nobody acknowledges it within ten minutes.”** Acknowledgment differs from notification acceptance and from incident resolution.
63. **“Resume normal delivery when the incident is resolved; schedule the retrospective and corrective work separately.”** Recovery need not wait for every follow-up to complete.

### Maintenance and other project styles

64. **“Group low-risk dependency updates weekly; process security fixes immediately.”** Classification selects batch membership or an expedited route.
65. **“Introduce schema compatibility first, migrate records next, and remove the old schema only when supported versions no longer use it.”** Long-lived obligations span releases and depend on authoritative compatibility observations.
66. **“Resume a partial migration from its checkpoint; never replay completed destructive steps.”** Checkpoints and safe resumption belong to the migration capability.
67. **“Cancel this release, but finish an already-started rollback and retain its evidence.”** Cancellation is scoped; recovery work can have an independent lifetime.
68. **“Publish the documentation site when checks pass. For printed manuals, wait for distribution confirmation after packaging.”** Delivery is project-specific and may require an external signal.
69. **“Stop an experiment after three inconclusive attempts and write up the result.”** Bounded inconclusive exploration is a legitimate terminal outcome.
70. **“When an input dataset is corrected, reprocess only affected partitions and invalidate results derived from the old input.”** The data host owns lineage and partition selection; workflow coordinates the requested work.

## 3. Minimum concepts behind the examples

| Concept | Why it needs a separate identity |
| --- | --- |
| Work and work revision | Desired outcome, acceptance conditions, dependencies, and ownership can change. |
| Execution attempt | Reassignment, retries, and late results must not overwrite one another. |
| Artifact and evidence | A source document, candidate, check result, or assessment proves something about a specific subject. |
| Decision | A choice has an authorized responder, declared outcomes, scope, and applicable revision. |
| External operation | Request acceptance, actual execution, completion, and uncertain outcome differ. |
| Policy run | The engine owns its pinned definition, progress, waits, intents, and execution history. |

These identities are related through typed references. AQ remains authoritative for work and assignments; evidence providers remain authoritative for their observations. The runner stores the inputs it used and its own progress rather than maintaining a competing task or Git database.

An agent may publish an attributed assessment such as “this change appears documentation-only.” It cannot turn that assertion into authoritative evidence that a check passed or a deployment completed. Domain services authenticate those facts. A human response cannot be manufactured by an agent emitting an event with the right name.

## 4. Candidate minimum command families

The engine needs a registry and a typed invocation mechanism, not built-in instructions for every command below. These nine families form a proposed AQ host vocabulary spanning the catalogue. Each family may expose several separately named, schema-checked commands with different authority and retry contracts.

| Family | Contract and purpose | Example coverage |
| --- | --- | --- |
| `context.read` | Obtain bounded observations, operation status, and evidence references with versions and freshness. Complete collection queries distinguish an empty result from an incomplete scan. | 3, 16, 35, 40, 55, 59 |
| `artifact.record` | Publish an immutable document, result, or attributed assessment; include subject revision and provenance. An authorized new record may supersede an earlier claim without rewriting history. | 4, 9, 14, 29, 37, 70 |
| `work.request` | Idempotently request implementation, investigation, planning, review, or repair; name inputs, required output schema, constraints, and a logical work key. Materializing a proposed task graph requires a separate typed operation in this family. | 5, 15, 17, 20, 26, 36, 45 |
| `work.change` | Request an explicit amendment, dependency change, priority change, hold, hold release, cancellation, or settlement under domain preconditions. These are distinct operations, never an unrestricted object patch. | 18, 19, 21, 23, 44, 47, 48 |
| `check.request` | Run a named machine check against an immutable subject and execution context. Return typed results and evidence; distinguish test failure, infrastructure failure, and uncompleted execution. | 29–35, 45, 52 |
| `decision.request` | Ask an authorized actor a bounded question with declared choices, subject revision, and expiry/default policy. The response enters through an authenticated decision service. | 10–14, 33, 41, 42, 58 |
| `message.send` | Submit a message to an authorized recipient with correlation and deduplication identity. Delivery and acknowledgment are separate observations. | 3, 7, 28, 48, 62 |
| `integration.request` | Prepare a candidate, request conditional publication, or reconcile an integration operation through distinct typed contracts. AQ integration and aq-git enforce source-control invariants. | 34, 43–46, 49, 51 |
| `release.request` | Request environment deployment, artifact promotion, rollout adjustment, or supported recovery through explicit extension commands. The release service owns environment exclusion and operation recovery. | 50–59, 65–68 |

Reviews fit `work.request(kind=review)` if their output contract expresses findings, dispositions, subject, and reviewer identity. They need not be a new engine primitive. Machine checks deserve a distinct host contract because their subjects, execution contexts, outcomes, and evidence reuse rules differ from free-form agent work.

Human decisions could share request-lifecycle machinery with work, but they retain a distinct authority contract. A coding agent finishing an assignment cannot satisfy a human-only decision.

Recording an artifact reference and issuing an attestation also need different schemas and authority checks. Possession of a result document does not establish that a test passed, a finding was resolved, or an authorized person approved it. The host validates finding dispositions, exceptions, and supersession relationships.

`integration.request` and `release.request` are capabilities registered by host modules, not permissions to run arbitrary shell or Git commands. Publication and rollback may need different grants. An integration command reaching aq-git does not bypass AQ's evidence and authorization checks.

The first coding pilot can omit release capabilities entirely. Core validation, simulation, and runner tests use fake registrations and require none of these production services.

### Shared command guarantees

Every effectful registration declares input and output schemas, typed outcomes, scope/authority requirements, sensitive fields, and its recovery behavior. A mutating request carries a stable operation identity and applicable expected versions. Accepted asynchronous requests return a durable handle; completion arrives through a correlated event or status observation.

Replay after a lost response reuses the same operation identity and payload. A deliberate new business attempt gets a new identity linked to its retry budget. Transport retry must not silently become another implementation attempt, review, deployment, or charge.

The result vocabulary must distinguish rejected admission, accepted/running, succeeded, failed, cancellation requested, canceled, and indeterminate wherever relevant. Each command declares its applicable subset. A completed command is not automatically a successful SDLC outcome.

Asynchronous families also need explicit cancellation or revocation operations where supported. Canceling work does not cancel its checks, a pending decision, or a deployment automatically. The relevant host reports whether cancellation was accepted, completed, refused, or too late; the policy defines what to do with late results. Notification delivery alone cannot resolve a decision or complete an obligation.

Budget accounting, task graph changes, required evidence checks, assignment fencing, and environment exclusion remain host-enforced. The runner can request operations and record refusals; it cannot grant itself permission by taking a workflow edge.

## 5. Candidate minimum event families

Event families organize registration. Their concrete variants retain distinct schemas, producers, and meanings.

| Family | Examples and use |
| --- | --- |
| `work.changed` | Work created or revised, dependency/hold change, execution started or stopped, result submitted, work accepted or canceled. Explicit variant and subject distinguish these transitions. |
| `artifact.recorded` | Spec revision, assessment, review findings, evidence supersession, external result. |
| `check.completed` | Check outcome with subject, suite/configuration, environment, execution identity, and evidence reference. |
| `decision.resolved` | Authorized choice, subject revision, actor, and decision-request identity. Expiration remains distinct from an answered decision. |
| `integration.changed` | Candidate prepared, conflict, publication applied or rejected, uncertain operation, reconciliation result. |
| `release.changed` | Deployment accepted or completed, rollout state changed, recovery completed, external operation became indeterminate. |
| `signal.reported` | Agent-reported blocker, monitoring observation, incident acknowledgment, external distribution confirmation. Registered subtypes define who can attest to each fact. |

Durable deadlines and scheduled occurrences are engine facilities with their own stable timer identities. They need not become fake worker tasks or an extra public command per schedule. A host may supply calendar occurrences through a registered event when it owns the calendar.

An event envelope carries producer identity, event ID, schema version, scope, subject and version, occurrence/observation times where relevant, and correlation/causation references. Authentication establishes identity; fields supplied by an agent do not establish authority. Stable event IDs deduplicate delivery, while domain keys identify repeated reports of the same underlying problem.

The producer persists an event in its own durable delivery record as part of the relevant state transition. The runner acknowledges only after durable acceptance. If a producer cannot atomically record an external fact, its adapter needs polling/reconciliation and explicit coverage limits. Reordered or delayed events require subject-version checks; timestamps alone do not establish causality.

The compiler checks trigger schemas. Runtime admission also checks scope and producer authority. Unknown schemas, invalid payloads, and unavailable capabilities have explicit failure dispositions.

## 6. Engine capabilities derived from the policies

### First useful release

| Capability | Required semantics | Examples |
| --- | --- | --- |
| Versioned artifacts and registry contracts | Pin each run to its definition and required contract versions; activate a new definition for new runs. Validate references and required capabilities. | 8–14, 38 |
| Typed conditions and values | Branch on recorded facts, compare fields, and preserve missing/unknown. Judgment is an external assessment. | 4, 8, 30, 35 |
| Durable command invocation | Persist intent before dispatch, validate results, resume asynchronous operations, reconcile uncertainty, and enforce adapter retry declarations. | 20, 26, 46, 59 |
| Event correlation and deduplication | Match exact work, attempt, candidate, decision, or operation; reject stale resolutions and duplicate effects. | 7, 14, 22, 27 |
| Waits and durable timers | Wait for correlated events or deadlines; close the event-arrival versus wait-registration race; retain timers across restart. | 5, 26, 41, 62 |
| Bounded iteration and parallel joins | Iterate finite collections; request multiple operations; express all/any and explicit failure handling. Bound execution and retained results. | 17, 31, 36, 37 |
| Run state and cancellation | Persist counters and progress; record cancellation intent; distinguish a stopped workflow from a stopped external operation. | 21, 26, 47, 67 |
| Scope and authority propagation | Carry authenticated caller and artifact constraints; preserve them through recovery; recheck authority at the host. | 12, 14, 36, 58 |
| Evidence references | Carry subject versions and observation metadata; validate declared comparisons without claiming to judge substantive evidence validity. | 29, 34, 38, 40 |
| Explainable simulation | Use the same transition semantics with mocked capabilities; report unknown branches and trace why actions would occur. | All examples |

All/any joins need explicit membership, timeout behavior, and treatment of failing or canceled participants. The first release can fix the participant set when a bounded fan-out begins. Dynamic membership and quorum changes can be added later.

Business retry limits must have an explicit scope. Two retries within one run are different from two retries across every event concerning the same bug. The latter requires an authoritative attempt count from AQ or engine-owned state with a stable logical key; starting a fresh run cannot reset the allowance accidentally.

Two playbooks can react to one event. The engine can guarantee deterministic transitions within each run, but cross-run effect order needs an explicit policy or host precondition. Host-owned holds have individual IDs, effects use expected versions, and concurrency limits bound event storms. There is no implicit last-writer-wins rule for contradictory policies.

### Capabilities to stage later or supply through extensions

| Capability | Why it can wait |
| --- | --- |
| Shared policy state and notification episodes | Needed for durable suppression and rolling counters across runs; requires scoped revisions, retention, and atomic outgoing intents. Initial workflows can use domain-owned incident/work identities. |
| Calendar rules and working hours | A Tuesday schedule is simpler than holidays, working-day deadlines, timezones, and daylight-saving transitions. A calendar adapter can supply occurrences initially. |
| Windowed aggregation | “Three unrelated tasks in an hour” and “thirty healthy minutes” require complete windows, distinct subjects, late-event rules, and freshness. A monitoring capability can first return an authoritative evaluation. |
| Dynamic quorums and participants | Ownership changes during a decision require explicit membership and invalidation rules. Start with a captured set. |
| Coordinated recovery branches | Explicit conditional recovery can use ordinary workflows initially. A general compensation framework should wait for demonstrated requirements; recovery actions can themselves fail or be irreversible. |
| Policy migration for active runs | Keeping existing runs pinned is the initial rule. Moving a paused run to a new definition requires a dedicated, validated migration contract. |
| Proving interactions between policies | Static validation can detect some contradictions but cannot prove arbitrary natural-language policies globally consistent. Begin with explicit priorities, host refusal rules, and scenario tests. |

Unsupported constructs should produce actionable diagnostics during authoring. They must not compile into a workflow that silently omits the difficult part.

### Responsibilities outside the runner

AQ owns the task dependency graph, readiness enforcement, assignment leases, worker placement, capacity, budget accounting, roles, and messaging. The engine's own workflow graph is distinct from that task graph. An invocation requesting a dependency change reaches AQ's graph validation rather than writing a second dependency model inside the runner.

aq-git and integration services own repository state, candidate creation, object retention, publication preconditions, and code-operation reconciliation. Deployment and data adapters own environment locks, resource reservations, migration checkpoints, and external observations. The runner can coordinate these capabilities without taking over their invariants.

An environment lock must protect the actual deployment while its outcome is unknown, even if the policy runner crashes. A workflow-only mutex is insufficient. Likewise, a policy saying “stay under budget” does not enforce a budget unless the execution host can reserve and limit the relevant resource.

## 7. Work backward through four policies

### Shared blocker investigation

**Policy:** “Create one investigation when several tasks report the same blocker. Resume affected work when the blocker is resolved.”

1. An authorized producer emits a blocker signal with project, work reference, and a stable blocker identity. An assessment supplies that identity if reports need semantic comparison.
2. The run calls `work.request` using a logical key derived from the project and blocker. Repeated reports reuse the investigation while attaching additional evidence through authorized operations.
3. `work.change` places separately owned blocker holds on affected work.
4. A correlated investigation result provides resolution evidence. Policy checks it or requests further judgment.
5. The workflow releases only the holds it owns. AQ recomputes readiness under all remaining constraints.

This needs ordinary commands, correlation, idempotency, and waits. It does not require a special blocker instruction or a model embedded in the runner.

### Review and exact candidate validation

**Policy:** “An independent review and required checks must support the candidate we publish.”

1. Request a review with the change revision, output schema, and independence constraints. AQ selects an eligible reviewer.
2. Request candidate preparation. The integration service returns immutable candidate identity and its source revisions.
3. Request the required checks against that candidate. Branch on typed outcomes and wait for all required evidence.
4. Compare the review's subject and scope with the candidate. Request an authorized applicability assessment or renewed review when necessary.
5. Request publication with candidate identity, destination precondition, and evidence references. The host rechecks the applicable obligations and authority.
6. A changed destination produces a conflict outcome. Prepare another candidate and reassess evidence; do not relabel prior evidence as passing for the new candidate.

This needs typed requests, parallel waits, joins, versioned references, and a bounded recovery branch. Review completion and check completion remain different facts.

### Revision-bound human exception

**Policy:** “Ask me only if the sole remaining failure is a known flaky check.”

1. Read the complete required-check set and applicable results for the candidate.
2. Establish the failure classification from a trusted test record or an attributed assessment allowed by project policy.
3. If the exception predicate holds, request a decision naming the exact candidate, check result, proposed exception, allowed responder, and expiry.
4. Wait for the response. Rejection, expiry, and approval have distinct paths.
5. Before publication, the host confirms that the candidate and exception scope still match. A new candidate cannot inherit approval merely because the run received it earlier.

This requires no universal approval stage. It requires a typed decision contract and subject identity.

### Deployment timeout with an unknown outcome

**Policy:** “If deployment times out, determine what happened before retrying.”

1. Persist the deployment request intent and stable operation ID, then invoke the registered release command.
2. A response timeout leaves the operation unresolved. It does not authorize a new deployment identity.
3. Query operation status through `context.read` or wait for the release service's durable completion event.
4. Continue if the original operation succeeded, take the declared recovery path if it failed, or request reconciliation/investigation if uncertainty remains.
5. The release service retains environment exclusion until it can safely resolve the operation. Workflow cancellation does not falsely declare the environment idle.

This tests the command recovery contract rather than requiring deployment mechanics inside the engine.

## 8. Authoring skill and tools

The authoring skill should help an agent discover capabilities, expose ambiguity, construct a proposal, and test the intended behavior. A useful small toolset is:

- `commands.list` and `commands.describe`: schemas, authority requirements, outcomes, and recovery guarantees.
- `events.list` and `events.describe`: payloads, trusted producers, scope, and correlation fields.
- `policy.validate`: structural errors, missing capabilities, unhandled outcomes, and unsupported constructs.
- `policy.simulate`: events, fake command outcomes, and an injected clock; return traces and unresolved interpretations.
- `policy.diff`: changes to triggers, required capabilities, conditions, effects, waits, and failure paths.
- `policy.package`: produce canonical artifact bytes from the exact source, semantic proposal, contract snapshots, and compiler version.

These tools do not need to invoke a model. The authoring agent supplies the interpretation and can iterate on diagnostics. Compilation can verify that a review result has a declared subject field; it cannot prove that the reviewer actually understood the change.

For each policy, the agent should produce a normal-path example, a failure or exception example, and any meaningful stale/duplicate/timeout example. For the review policy, useful cases include a new commit after approval, an unrelated check completion, duplicate results, and a reviewer becoming ineligible.

## 9. Recommended first slice and decisions

Start with a complete coding loop: requested work → implementation result → conditional review → candidate checks → conditional integration → bounded repair or supervisor escalation. This exercises commands, typed outcomes, parallel waits, identity, deadlines, recovery, and evidence without requiring a release platform.

That pilot needs context, artifacts, work requests/changes, checks, integration, and decisions when a policy asks an owner or supervisor to choose. Messaging makes the interaction usable without introducing a new engine primitive. Deployment remains an optional host extension. Start with conservative invalidation after relevant input changes; selective evidence reuse can follow when its assessment contract is settled.

Treat the catalogue as an acceptance-case library, not a demand to implement seventy special workflows. Every example should eventually have one of three dispositions: expressible with the current language and registered capabilities; expressible when a named host capability is added; or blocked on a specific future engine feature.

The next design decisions are the exact command contract format, the minimum expression language, how policies reference shared project definitions, and which cross-run state needs engine ownership. Those should be tested against these examples before fixing the artifact schema.
