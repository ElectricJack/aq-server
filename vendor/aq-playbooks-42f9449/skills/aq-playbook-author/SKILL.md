---
name: aq-playbook-author
description: Translate project policy into bounded aq-playbooks proposals, validate and simulate them, and package immutable artifacts. Use for authoring this engine's workflows, not general AQ administration.
---

Read the repository's `docs/specs/0002-implementation.md` and model `FORMAT.md` for the executable format. Use `aq-playbooks --help` for the actual installed CLI, and inspect examples before constructing a proposal.

Discover the available command/event catalog and callable artifacts. The bundled catalog is a synthetic pilot contract set; a production host must supply its actual registrations. Discovery does not grant execution authority.

Bind the Markdown source and explicit semantic proposal. Resolve terms such as “ready,” “independent reviewer,” and “required checks” against captured definitions and registered contracts. Record consequential unresolved interpretations as proposal questions; compilation must not invent answers. Preserve source mappings where useful.

Use native calls, joins, waits, branches, and finite iterations for coordination. Keep work, check evidence, model assessments, human decisions, and notification acceptance distinct. Bind evidence and decisions to exact subject revisions. A source document cannot grant capabilities, and a child cannot expand its caller's scope.

Every branch needs its business outcomes and explicit unknown/error paths. Retries need finite budgets; transport replay preserves operation identity. A timeout does not establish that an external operation stopped. Owned children and asynchronous operations must settle before the parent terminates. Independent starts require an explicit lifetime choice.

Validate the exact proposal. Exercise normal, failure, and relevant stale/duplicate/deadline cases with fake responses and injected time. Missing mocks remain unresolved. Compare semantic changes with the prior artifact, especially authority, effects, dependencies, and failure behavior. Preserve simulation limitations in the review.

For parallel results used downstream, declare Join `member_keys` so the compiler can preserve each participant's output type. Choose exact dependency pins or explicit compatible catalog selection; admitted runs retain their frozen closure. Sensitive persistence requires an explicitly configured host protection keyring; source prose cannot enable it.

Package only the reviewed exact inputs. Installation, activation, and execution are separate actions; perform them only within the user's existing authorization and the host's current authority checks. Do not invent a universal human gate. Report the artifact identity and any unimplemented host capability required by the policy.
