# Deliver a task that uses multiple project workspaces

The Home project owns the task and its completion. Each approved sibling checkout has a frozen project revision, repository, binding generation and commit. After a stopped attempt, AQ verifies changed sibling heads against the central Git refs. The delivery plan includes the retained Home result and every read-only sibling commit. A conflicting or missing result leaves the plan incomplete.

A changed target needs its own delivery evidence. AQ can reuse a verified merge for the exact repository, binding generation, branch and retained head. A retry for another repository keeps that evidence. Integrated checks apply to the complete checkout set, so a changed writable or read-only input invalidates the previous check result.

## Owner evidence API

The human installation owner can use `GET /api/projects/{home}/tasks/{task}/results/{report}/workspace-delivery` to read the checkout heads, missing inputs, target acceptance and retained publication state. Mutations use `POST` on the same path with a fresh UUID `operation_id` and a typed `action`. Use the latest held result and repeat the same operation ID to retry that action. The normal owner authentication and request header apply.

| Action kind | Fields | Effect |
| --- | --- | --- |
| `integrated_checks` | `heads` map from project ID to exact commit, `verdict` (`passed` or `failed`), `evidence` reference | Records the result of checks run with the exact checkout set. |
| `publish` | `target_project_id`, `head` | Uses the installed target's pull-request policy to publish that branch under a lease and open or find its PR. |
| `verify_merge` | `target_project_id`, `head`, `pull_request` number | Reads the forge and records a merged PR at the exact head and configured branch. |
| `owner_acceptance` or `external_delivery` | `target_project_id`, `head`, `evidence` reference | Records a target-specific attestation where the target policy allows external or owner acceptance. |

For a configured pull-request target, AQ requires verified merge evidence. The owner publication action stops at the PR. When the Home result requires human review, its playbook waits for a verified external merge at that revision. Workspace evidence and publication mutations require the human owner, including when a project supervisor has delegated Home access.

When every sibling in the plan is read-only or unchanged, the merged result contains only the Home change, so the Home project's own required checks on its PR are the integrated check: AQ treats integrated checks as satisfied without an attestation, and the evidence read reports `integrated_checks.satisfied_by` as `home_required_checks`. A plan with any changed sibling still needs the attestation (`owner_attestation`). The current runtime has no configured verifier for a multi-repository input set. A human owner records the integrated check outcome from the run at the exact refs. While delivery's `pull_request.merge` waits for that attestation, the task's work read shows the readiness reason `workspace_integrated_checks_required`, naming the missing `integrated_checks` attestation and the owner's evidence route. The existing playbook check interface can pin a single source and its production executor is currently unavailable. The Home policy actor can continue once the integrated attestation and independent target evidence are present.
