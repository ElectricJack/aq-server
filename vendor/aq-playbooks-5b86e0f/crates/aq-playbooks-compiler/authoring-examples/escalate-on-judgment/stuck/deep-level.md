# Who takes a task

When a task is ready, decide how strong a model it needs and which agents may
take it. If I have chosen that myself, leave the task alone.

A task that has already failed three times for a real reason is never given
to an agent again. Leave it for me, and say why on the task.

For every other task, judge how much damage the change could do if it is
wrong. High risk is anything that touches stored data, permissions or money.
Low risk is docs, wording and tests. Everything else is medium risk.

- Low risk work is Simple, high effort, and any agent may take it.
- Medium risk work is Standard, high effort. When the work is small, its
  requirements are settled and a test checks the result, Simple, high effort
  is enough.
- High risk work is Deep, high effort, because mistakes there are costly,
  and only Claude or Codex may take it.

If the risk can't be told, treat the task as high risk and say that this was
assumed. That includes when the judgment fails or doesn't come back within three
minutes.

Always say on the task why it got the level it got.
