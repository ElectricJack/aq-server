# Routing

Before a task is handed to a worker, decide how capable a model it needs and
which kinds of agent may take it. Don't pick the worker, the machine or the
exact model. Scheduling does that afterwards, within the limits set here,
from whatever is available at the time.

Route a task when it becomes ready, and again when it is retried or reopened.

## How to decide

Start from what kind of work it is, then adjust for risk.

**By kind of work**

- Features, bug fixes, refactors, tests and docs are Standard, high effort.
  Most tasks belong here.
- Chores and syncs are Simple, high effort.
- Specs, designs, implementation plans and reviews of someone else's work are
  Complex, high effort.
- Research is Standard, high effort, unless it is clearly harder, in which
  case it is Complex, high effort.
- Deep, high effort, is only for analysis, planning, spec writing, design or
  review that needs advanced mathematics or unusually hard reasoning. A big
  task is not a Deep task, and neither is a risky one. Whether a task needs
  that is your judgement of the task itself, not of how it was labelled.
- Analysis that doesn't need Deep is Standard, high effort.
- Art is Standard, high effort. Visual design is a design, so it is Complex,
  high effort.

If whoever filed the task asked for a level, respect it, within these limits.
Tests, docs, chores and syncs never go above Standard, whatever was asked.
Features, bug fixes and refactors never go above Complex. Asking for Deep is
not enough to get it: the task still has to need it by the rule above, and
otherwise it gets Complex, high effort, or its limit when that is lower. When
a request is cut back to a limit, use high effort at that level. If the level
asked for isn't available, use the same level at the next lower effort that
is available, and if there is none, the level below at high effort. The risk
and failure rules still apply to it, as to any level asked for.

Where these rules leave a choice open, or two of them point different ways,
use your judgement of the task itself, and prefer the cheaper level that is
adequate.

A bug fix that has already failed once for a real reason moves up to Complex,
high effort.
Failed verification or the agent giving up count. An outage, a lost worker or
review feedback on finished work do not.

**By risk**

Risk is how much damage the change could do if it is wrong. Judge it
separately from how hard the work is, and from how well the task is written.

- High risk: merging and publishing, CI gating, crash and session recovery,
  git branch handling, database migrations and stored data, authentication,
  permissions and secrets, money and live trading, deleting anything.
  Treat it as very high when a mistake would be hard to undo or would reach
  outside the repository.
- Medium risk: ordinary changes to shared code that other work depends on.
- Low risk: docs, UI copy, isolated components and tests, where a mistake is
  visible and cheap to undo.

When a task touches several areas, go by the riskiest. When you can't tell,
call it medium.

Medium and high risk work is at least Standard, high effort. Very high risk
work is at least Complex, high effort, unless it is narrow and covered by a
test, in which case Standard, high effort is enough.

A risk minimum wins over the limits by kind of work. Those limits cap what
was asked for and what the kind alone would give; they never pull a task
below its risk minimum. So a very high risk sync that is not narrow and
covered by a test gets Complex, high effort.

Never raise the level because providers are busy, and never use a task's
priority to choose anything. Priority only orders the queue.

## Who may take it

Any kind of agent that Settings offers may take any task. Don't restrict a
task to particular kinds of agent, don't prefer one kind over another, and
never hold a task back to wait for a particular one.

Never spend Deep on bug fixes, escalated or not.

"Narrow" means the requirements are settled and the files to change are named
or obvious. "Covered by a test" means an existing or specified test checks
the result. Work that needs manual or end-to-end verification is not covered.

## When it goes wrong

- If the risk or the kind of work can't be worked out, don't block the task.
  Use what is known. An unknown risk counts as medium, and you say that it
  was assumed. An unknown kind gets Standard, high effort, unless whoever
  filed the task asked for a level: then use that level, but never Deep, and
  Complex at most. A risk that is known still applies in full.
- If nothing allowed can take the task right now, hold it until something can.
  That is a wait, not a failure.
- If nothing allowed could ever take it, leave it unrouted and visible, say
  why on the task, and raise it for my attention. Never leave a task waiting
  with no explanation.
- Always record on the task what was decided and why: the level, who may take
  it, and anything that raised or lowered the level.
