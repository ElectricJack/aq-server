---
name: aq-reviews
description: Review AQ v2 task results or documents and inspect or resolve existing AQ review decisions when authorized.
---

# AQ v2 reviews

## Review the exact subject

For a recorded result, use the result ID and exact revision. For a document
or PR, record the exact revision or commit and use the review channel named in
the assignment. Keep author and reviewer roles distinct when independent
review is requested.

```sh
aq result show --result-id "$result_id" --revision "$revision"
aq result comments --help
aq result comment --help
```

Tie each finding to the revised artifact, cite the affected location, and
explain the failure or acceptance gap. Mark blocking findings separately
from nits. Where the user explicitly requests a cross-family review, record
the author and reviewer families, review the actual fixed revision, and stop
after at most three rounds. Leave unresolved blockers with the decider. On a revision, record each finding as fixed, rebutted with evidence,
or deferred with the agreed scope change. Preserve material dissent for the
decider. For anchored result feedback, use the current subject version and
anchor schema shown in `aq run result.comment --describe`.

## Inspect and decide an existing gate

Host examples omit connection flags. Use the flags in `aq-cli`.

```sh
aq decisions list --state open
aq decisions show "$decision_id"
aq decisions respond --help
```

If explicitly authorized to decide, build the response file from the current
decision's subject and expected revision. Inspect
`aq run decision.resolve --describe` for the payload. Submit with
`aq decisions respond "$decision_id" --request response.json` and read the
decision back. For uncertainty, inspect the operation before another write.
A human-only decision stays with the human.

V2 review decisions are created by the owning workflow. For a standalone
spec, plan, or proposal without a configured review workflow, provide the
draft to the requested human review channel, or use the managed task's
result handoff. Record that the artifact is awaiting review, and only report
an AQ decision ID when the workflow actually creates one.
