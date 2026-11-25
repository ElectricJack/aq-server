# Deliver a finished task

When a task has a result, publish its branch and build the environments the
result changes. Wait up to thirty minutes for the build. Then open a pull
request for the result. Wait up to thirty minutes for the pull request's
checks.

If the build fails, or a check fails, send the task back to its agent, to
start again from the current main, as long as it has been sent back fewer
than the number of returns in the facts (`max_returns`). A failed build is
sent back with the step that failed. When it has been sent back that many
times, hold the task and tell the supervisor.

If every check passes, merge the pull request by squashing it. When it is
merged, admit the environments that were built, and complete the task. If the
build changed no environment, there is nothing to admit. Wait up to five
minutes for the admission. If the environments cannot be admitted, tell the
supervisor and complete the task all the same: the pull request is already
merged.

If the pull request conflicts, is closed or waits too long, or cannot be
merged, hold the task. Hold it too if the build does not answer in time.

When a new base environment has been admitted for a project, rebuild the
project's environments on it, and wait up to thirty minutes for the build.
Admit each one that builds. If a rebuild fails or does not answer in time,
or the environments cannot be admitted, tell the supervisor. The project keeps
the environments it has.
