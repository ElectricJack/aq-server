---
name: aq-mini-projects
description: Turn an explicitly requested, bounded ideation brief into a critiqued roadmap for human review in an AQ v2 task.
---

# AQ v2 mini-projects

Run one proposal, one critique and at most one synthesis revision in the
requested task. Use serial passes unless the requester explicitly asks for
sub-agents and the harness supports them. For a managed task use `aq whoami`
and `aq task show self`. Respect the assignment's deadline and budget. If
unspecified, use the quick-mode limits of 90 minutes, three subordinate agents
when explicitly requested, and 32 KiB each for the brief and role artifacts.

1. Freeze the brief as a local UTF-8 artifact within the task's limits.
   Record its SHA-256, source, scope, constraints, rubric, and repository HEAD
   if code-grounded. Use the same bytes and revision for every pass.
2. Propose viable options with tradeoffs, cost, dependencies, risks and
   measurable acceptance. Distinguish evidence, assumptions and open questions.
3. Critique the proposal against that same brief. Mark blocking findings and
   nits, cite evidence, and give the concrete failure and possible correction.
4. Synthesize a prioritized roadmap. Record each finding as fixed, rebutted
   with evidence, or deferred with the explicit scope change. Preserve
   unresolved blockers and material dissent. The facilitator owns the final
   draft. If a required role or artifact is missing, record partial progress
   and the blocker without inventing approval or a completed review.

Keep the final Markdown draft in the authorized artifact location, with
brief hash, source revision, role provenance and decision points. In a
managed workspace, follow `aq-workspaces-and-git` to hand it off with the
result; include the path and review status in the conclusion. If the
request names a human review channel, use it when authorized. An existing
AQ review decision is handled through `aq-reviews`. Without a configured
gate, report the plan as awaiting review. A completed plan does not file
implementation work or approve itself.
