# Deliver code changes for the scratch project

Apply this rule to a result-ready event for project `e0000000-0000-4000-8000-000000000002`. For any other project, end the run immediately without taking action.

Read the task named by the event. Use the event's result ID for every decision. If the task has moved to a different result, leave the newer result to its own run. A successful code result must have changes and a task branch head. Otherwise hold this result with reason `policy` and tell the project's supervisor why.

Publish the task branch, then open or find its pull request against the default branch. Wait up to 1,800,000 milliseconds for the repository's required checks. Use the same PR when an agent returns with a repair. During each asynchronous operation, wait for the host's recorded completion before taking the next step.

- When checks pass, merge using a merge commit and the task head read for this result. Complete the task after the host confirms that merge. If the host is still reconciling the operation, wait for its outcome.
- When checks fail and the task has fewer than three returns, send it back with reason `checks_failed` and the failing check names. Resume from the task branch.
- When the PR conflicts with the target or GitHub confirms that its tracked result head is behind the target, the delivery commands report the conflict outcome. Below three returns, send it back with reason `merge_conflict` and request an update from main. Keep the task branch and the existing PR, and await checks on the repaired head.
- On either failure after three returns, hold the task with reason `returns_exhausted` and notify the supervisor.
- On a check timeout, end the run. The next result-ready observation rechecks the same work. A timeout consumes no repair attempt.
- On a publish lease conflict, closed unmerged PR or refused merge, hold the result with the applicable reason and notify the supervisor. Include the operation outcome in that message. A refused or uncertain operation cannot authorize completion.

For each hold, use a notification identity derived from the result ID and the reason so a replay sends one message. This pilot finishes at merge. Environment rollout uses the separately configured deployment policy.
