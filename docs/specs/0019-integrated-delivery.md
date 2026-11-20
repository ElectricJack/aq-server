# 0019 Integrated delivery, dependency ordering and deployment

Builds on [0016 Delivery by playbook](0016-delivery-by-playbook.md). The pilot uses one task branch and one PR into the project default branch. Multiple epics can share that branch. Graph records explicit prerequisites independently of the parent ticket.

The [repository enrollment runbook](../operations/delivery-enrollment.md) covers the GitHub App, required checks, AQ bindings and the evidence to collect before widening a pilot.

## Result to merge

1. On aq.work.result_ready, read the current attempt and result. The initial code policy holds an unsuccessful or unchanged result and notifies its supervisor once. A report-only project may author a different rule.
2. Publish refs/heads/aq/<task-id> with its lease. Open or recover the same PR for that repository, task branch and default branch. Keep its identity through repair.
3. Await required GitHub Actions checks for the current candidate, using repository protection and any extra checks named by the project. A protected check pinned to a GitHub App needs a report from that App. A named check that has not reported stays pending. When neither a check requirement nor any observed check exists, return pending until the deadline. For the pilot configure required checks, enforce the configured producer and require an up-to-date branch or equivalent GitHub enforcement of the tested combination. The App must respect that protection. A change in the target that invalidates checks requires revalidation.
4. On red return the failed check names. On conflict return with update_from_main. Use the task branch for repair. After three returns hold and notify once. A 30-minute check timeout ends that run and permits a later result-ready observation. Unknown remote outcomes require reconciliation before retry. A closed unmerged PR or policy refusal holds with a reason visible to the supervisor.
5. Merge with the merge method and expected task head. Record repository, PR, task, attempt/result, expected head, target ref and actual merge commit from GitHub. Reconcile a lost reply against that same PR and head. Replaying cleanup must preserve the merge evidence. Delete the delivered task branch on origin and central aq-git, then settle through work.complete and the validated Graph receipt.

## Offer a dependent task

A completed prerequisite with a code delivery contributes its merge SHA to the dependent's required base for the same repository. On first offer, retain the current default branch head and verify that it contains every such SHA. Refresh from origin when the cached head fails this check. Leave the dependent ready but unoffered with an observable reason if the refreshed head still excludes a required commit. Independent siblings can run while this task waits. Report-only prerequisites contribute completion evidence. Cross-repository dependencies name the package version, contract or artifact they consume. A dependency requiring deployed behavior names the environment and waits for the corresponding healthy deployment.

## Merge to environment

The project delivery document selects the permitted deployment target and approval rule. The initial choice is automatic deployment to an opted-in development environment and explicit production promotion. Projects that only publish a library may finish at merge. Preserve the project's existing release mechanism, including version tags and signed manifests where required. Record the merge first, resolve the build from that revision, then bind the immutable artifact digest to its source revision.

A deployment operation is keyed by project, environment, revision and artifact. Persist its intent before dispatch. Expose queued, running, healthy, failed and superseded outcomes with provider operation identity, observed revision or digest, timestamps and a diagnostic. A request or workflow success alone cannot prove the target is running the artifact. Reconcile against the provider operation and the environment after restart. Serialize writes per environment and fence delayed retries against the latest desired generation. A retry reuses the artifact. Rollback is a new authorized desired generation selecting a previous artifact. A deploy failure leaves the merge recorded and independently retryable.

## Implementation and pilot

- First implement empty-check handling and preserve the actual merge SHA. Extend a published host contract with a new immutable catalog version where its output shape changes. Retain decoding for historical receipts with no SHA and recover evidence from GitHub before using one as a dependency or deploy input.
- Bring delivery and current routing/plugin capabilities together on a compatible host, preserving existing catalog fingerprints. Qualify the server, worker, aq-git, dashboard and App configuration together. The [scratch Settings document](../operations/scratch-delivery-policy.md) scopes merge decisions to the pilot project. Qualify and activate that document on the pilot host.
- Implement the prerequisite ancestry check and deployment operation separately. Expose the waiting reason, PR, merged revision, environment and observed rollout on the task. Reconcile old retained results before enrolling each project. Recover the existing PR/merge where identifiable and hold ambiguous results for the supervisor.
- On scratch run two concurrent epics and a dependent pair. Exercise red, conflict, missing checks, stale cached main, moving target, interruption after merge and interruption after deployment dispatch. Expect one PR and merge per task, no stale dependent offer, one rollout per artifact/environment and observed healthy revision. A newer healthy deployment survives a delayed older retry. Pilot first, then enable projects individually.
