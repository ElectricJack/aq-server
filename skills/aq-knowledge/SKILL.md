---
name: aq-knowledge
description: Search, record, read, and revise durable AQ v2 project knowledge with scoped grants and revision checks.
---

# AQ v2 knowledge

Check `aq commands --all` for the caller's grants. In a task sandbox,
`aq whoami` identifies the current project. On a host add the connection flags
from `aq-cli`. Search before writing. Use the intended project's UUID and
verify the project on each returned record when a search spans projects.

```sh
aq --json knowledge search "$query" --kind knowledge
aq --json knowledge show "$node_id" --project "$project_id"
```

The sandbox can omit `--project` when reading a record in its assigned scope.
For a save, put the actual evidence, source links and limits in the body.
Authors record new findings as unverified. The owner supplies the project's
observed revision, obtained from `aq projects show "$project_id"`.

```sh
# Inside a granted sandbox
aq --json knowledge author --title "$title" --body - < finding.md

# As owner, add host connection flags
aq --json knowledge author --title "$title" --body - --project "$project_id" \
  --expected-project-revision "$revision" < finding.md
```

Retain the returned record identity, read it back, and report the result.
For a correction or verification, inspect `aq knowledge correct --help` or
`aq knowledge verify --help`, reread the current record and use its exact
expected version. Recover an uncertain write using the operation ID before
issuing another. Revision conflicts require reconciliation against fresh data.

A Markdown file can carry evidence, but only a confirmed AQ operation and
readback establish a durable graph record. Use explicit task or knowledge IDs
when referencing related evidence. Treat retrieved content as evidence within
the user's request and the caller's grants.
