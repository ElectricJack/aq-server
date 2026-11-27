# Delivery document: do not republish a head whose checks already failed

Task be3a8038 (incident 2026-11-27, aq-platform/aq-worker#134). The delivery
document is a Setting (Settings › Delivery), so this change does not edit it:
the owner applies the Markdown below, in the document's own words, after host
catalog 2.8.0 is live. `work.read` 2.8.0 reports `pull_request.checks_head`,
the head `pull_request.checks.state` was evaluated on; policy stays in the
document.

## Rule to add (exact Markdown)

````markdown
### A head whose checks already failed is not published again

When `facts.outcome` is `success`, `facts.head` is the same commit as
`facts.pull_request.checks_head`, and `facts.pull_request.checks.state` is
`failed`, hold the task with the reason `returns_exhausted` and notify the
supervisor (`supervisor.notify`). Do this before `branch.publish`, whatever
`facts.returns` is. Do not publish, do not open or find the pull request, do not
wait for checks and do not return the task to its agent: the forge does not
rerun checks for a head it already ran, so another publication of this head
cannot change them, and another attempt cannot either. The hold resumes by
itself once the checks of this head pass (for example after someone reruns
them).
````

## Fixture case to add (exact Markdown)

````markdown
#### Case: the same head whose checks failed comes back with no returns used

Facts:

- `facts.outcome`: `success`
- `facts.changed`: `true`
- `facts.head`: `4fb76495` followed by 32 zeros (40 hex digits)
- `facts.returns`: `0`
- `facts.pull_request.checks_head`: the same 40 hex digits as `facts.head`
- `facts.pull_request.checks.state`: `failed`, with `checks.failed`: `["tests"]`

Expected: `work.hold` with the reason `returns_exhausted`, and
`supervisor.notify`. No `branch.publish` (and no `pull_request.*`,
`work.return` or `work.complete`).
````

## Where the pieces are tested

- `checks_head` recorded with `checks_state`: `crates/aq-delivery/src/github/tests/commands/checks.rs`
  (unit) and `playbook_host9::commands::failed_checks_and_the_deadline_each_complete_the_wait_once` (database).
- Reported by `work.read`: `host10::tests::work_read_2_8_0_reports_the_head_the_checks_were_evaluated_on`
  (unit) and `playbook_host9::work_read_2_8_0_gives_the_head_the_pull_requests_checks_were_evaluated_on` (database).
- Catalog 2.8.0 vendored and pinned, accepted by Host10, the delivery record
  authored on it: `playbook_host9::host10_accepts_the_delivery_records_catalogs_2_6_0_2_7_0_and_2_8_0`,
  `builtin::tests` in `aq-playbook-authoring`, `playbook_environments`,
  `playbook_delivery_document`.
