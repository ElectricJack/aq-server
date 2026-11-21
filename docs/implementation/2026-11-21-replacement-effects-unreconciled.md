# Recovering a stopped attachment task

Task `8c6ba979-993d-4611-b5ab-22228ec25e73` ended with a failed result on November 20, 2026. The worker preserved 15 changed files in central aq-git at `e829f0252e69c021473dbb4df1aac1aeb96f5402`. Its final answer was “Waiting for the build to finish.” The worker requires an explicit changes report to record success. Its summary generator also prints “Outcome: done” for any final answer without an explicit blocked line.

Delivery recorded a policy hold on report `c1918b17-e647-46e0-9a42-0fcb0dd4a8ef`. An owner retry subsequently returned `replacement_effects_unreconciled`. The held attempt had no Graph operations or mailbox operations. The server had skipped the reconciliation step because only an explicit return for rework ran it. As a result, the attempt still held its task with source retention pending and effects unknown, despite the recorded Git push.

Four other returned attempts had a terminal mailbox operation. The acceptance check for a read-only, mail-free result also prevented those attempts from recording settled effects for a replacement.

## Inspect and retry

Inspect the current attempt with `aq attempt show --attempt-id <attempt-id>`. Compare its stop, source retention, effect receipts, and replacement blockers with the task's current result. A revision conflict requires reading the task again. A report that was superseded cannot authorize a retry.

Use `aq tasks retry <task-id> --project <project-id> --revision <current-task-version> --reason '<why another attempt is appropriate>'` after establishing that the task should resume. An admitted retry of a failed result with a delivery policy hold checks the current report, the stopped attempt, its retained source and terminal operations. The replacement owner rechecks those facts and budgets before release. A verified source lets the successor continue with the retained work. The retry operation receipt and the successor's session record show release and launch separately.

For returned results the ordinary sweep records available receipts. Pending or unverified operations still require reconciliation by their owning service. A completed or revoked attempt cannot qualify for the held-result retry path.

## Supervisor dispatch

The delivery notice for the attachment task reached the aq-server project conversation on November 20 at 20:08 UTC. The supervisor replied 55 seconds later asking for the task or result ID. Delivery notices originating in a result event now carry the project, task, revision, attempt, result, and stored hold reason. Message deduplication still uses the result and its hold reason.

## Recording an independent review

Set the task type or label to `review-only`. In its body, put a single line of the form `Review target: <implementation report UUID>@<64-character report revision>`. Provide the reviewer with the exact source or a verifiable patch, along with its base and retained commit. A blocked review stays incomplete.

On the first line of the reviewer's final result, record `Review verdict: approved <report UUID>@<report revision> <retained commit>` or use `changes_requested` in place of `approved`. Give findings and the checks performed, then include the usual `Changes: none` on its own line. The server compares the reviewer's recorded successful report and unchanged source with the assignment version, the target project's immutable revision pin and the target's independently recorded Git head. Completing a review records the disposition of the review assignment. Implementation delivery still uses its own protected checks and merge.
