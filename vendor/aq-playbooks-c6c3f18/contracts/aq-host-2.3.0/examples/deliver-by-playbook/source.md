# Deliver a finished task

When a task has a result, publish its branch and open a pull request for it.
Wait up to thirty minutes for the pull request's checks.

If every check passes, merge the pull request by squashing it, and complete
the task.

If a check fails, send the task back to its agent, to start again from the
current main, as long as it has been sent back fewer than the number of
returns in the facts (`max_returns`). When it has been sent back that many
times, hold the task and tell the supervisor.

If the pull request conflicts, is closed or waits too long, or cannot be
merged, hold the task.
