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
- Research is Standard, high effort, unless it is clearly harder.
- Deep is only for analysis, planning or spec writing that needs advanced
  mathematics or unusually hard reasoning. A big task is not a Deep task, and
  neither is a risky one.

If whoever filed the task asked for a level, respect it, but don't go above
Standard for tests, docs, chores and syncs, or above Complex for ordinary
implementation work.

A bug fix that has already failed once for a real reason moves up to Complex.
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
work is at least Complex, unless it is narrow and covered by a test, in which
case Standard, high effort is enough.

Never raise the level because providers are busy, and never use a task's
priority to choose anything. Priority only orders the queue.

## Who may take it

- High and very high risk work goes only to Claude or Codex.
- Medium risk work goes to Claude or Codex. It may go to a local model only
  when the task is narrow, covered by a test, and Standard.
- Low risk work that is narrow and covered by a test should go to a local
  model first, to save Claude and Codex usage.
- The free hosted models take only trivial work: low risk, narrow, covered by
  a test, and Simple.
- Anything not mentioned here, Gemini for now, takes only low risk work.
- For ordinary implementation, prefer Codex when it is available, to save
  Claude usage.
- Design and planning go to Claude first, and to Codex only when Claude can't
  take them.
- Art and visual design go to Codex, and wait for it rather than going
  elsewhere.
- Keep the Deep Claude models for design, review and deep analysis. Don't
  spend them on escalated bug fixes.
- Repairs to a failed integration or build never go to a local or free model.

"Narrow" means the requirements are settled and the files to change are named
or obvious. "Covered by a test" means an existing or specified test checks
the result. Work that needs manual or end-to-end verification is not covered.

## Widening what the free models may take

A free hosted model earns ordinary narrow, low risk, Standard work after ten
clean tasks in a row: each landed and, in the two weeks after, was not
reopened, reverted or repaired. A failed task resets the count. Two reworked
tasks in its last ten send it back to trivial work only. Each model earns its
own record. Widening is a change to this document that I approve, never
something that happens by itself. Free models never take medium risk work or
above, however good their record.

## When it goes wrong

- If the risk or the kind of work can't be worked out, don't block the task.
  Route it as medium risk at Standard, high effort, on Claude or Codex, and
  say that the risk was assumed.
- If nothing allowed can take the task right now, hold it until something can.
  That is a wait, not a failure.
- If nothing allowed could ever take it, leave it unrouted and visible, say
  why on the task, and raise it for my attention. Never leave a task waiting
  with no explanation.
- Always record on the task what was decided and why: the level, who may take
  it, and anything that raised or lowered the level.
