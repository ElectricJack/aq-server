# Delivery document: do not republish a head whose checks already failed

Tasks be3a8038 (incident 2026-11-27, aq-platform/aq-worker#134) and c05530d6.
The delivery document is a Setting (Settings › Delivery), so this change does
not edit it: the owner applies the Markdown below, in the document's own
words, after host catalog 2.9.0 is live.

Catalog 2.8.0 reported the head the checks were evaluated on as
`pull_request.checks_head`, but `pull_request` is nullable and a playbook
reference cannot traverse a nullable value (aq-playbooks `schema_path`:
"cannot traverse scalar/nullable reference"), so no playbook could read it, nor
`pull_request.checks.state`. The delivery-document authoring run of
2026-11-27 (delivery source run 27) refused the earlier rule for that reason.
`work.read` 2.9.0 adds `facts.failed_checks_head`, always present: the head
whose checks were last evaluated and failed, or `""` when there is none. The
rule below reads only that fact beside `facts.outcome` and `facts.head`;
policy stays in the document.

## Rule to add (exact Markdown)

````markdown
### A head whose checks already failed is not published again

When `facts.outcome` is `success`, `facts.failed_checks_head` is not empty, and
`facts.head` is the same commit as `facts.failed_checks_head`, hold the task
with the reason `returns_exhausted` and notify the supervisor
(`supervisor.notify`) with a notification of its own, saying that the result's
head is one whose checks already failed and that it is held. Do this before
`branch.publish`, whatever `facts.returns` is. Do not publish, do not open or
find the pull request, do not wait for checks and do not return the task to
its agent: the forge does not rerun checks for a head it already ran, so
another publication of this head cannot change them, and another attempt
cannot either.

A held result is settled: delivery does not look at it again by itself. The
supervisor or the owner moves the task on, either by rerunning the checks of
that head, or by retrying the task so that its agent makes a new result.
````

## Fixture case to add (exact Markdown)

````markdown
#### Case: the same head whose checks failed comes back with no returns used

Facts:

- `facts.outcome`: `success`
- `facts.changed`: `true`
- `facts.head`: `4fb76495` followed by 32 zeros (40 hex digits)
- `facts.returns`: `0`
- `facts.failed_checks_head`: the same 40 hex digits as `facts.head`
- `facts.pull_request`: open, `checks.state` `failed` with `checks.failed`
  `["tests"]`, `checks_head` the same 40 hex digits as `facts.head`

Expected: `work.hold` with the reason `returns_exhausted`, then
`supervisor.notify`. No `branch.publish` (and no `pull_request.*`,
`work.return` or `work.complete`).

#### Case: a new head after checks failed on an earlier one is published

Facts: as in the case above, except `facts.head` is `5a1c0000` followed by 32
zeros, a different commit from `facts.failed_checks_head`.

Expected: `branch.publish`, as for any other successful result. No
`work.hold` before it.
````

## draft.json steps

Add these three steps to `steps` of the delivery draft, and point the edge
that leads to the `branch.publish` step today (in the published 2.3.0 example,
`read`'s `observed` edge) at `failed_head` instead. `failed_head`'s `false`
edge goes where that edge went: in the example, `publish`, the step that
invokes `branch.publish`; use that step's name in the live draft if it differs.
`held` and `failed` are the draft's existing `return` steps of the outcomes
`held` and `failed`. The steps use the bindings `facts_of` (the `work.read`
step's) and the entry input's `task`, `project` and `result`, as the example
does. The notification has its own text, so the supervisor can tell it from
the one sent when returns are exhausted.

```json
{
  "failed_head": {
    "op": {
      "kind": "decide",
      "predicate": {
        "op": "and",
        "values": [
          {
            "op": "eq",
            "left": {"op": "ref", "root": "facts_of", "path": ["facts", "outcome"]},
            "right": {"op": "literal", "value": "success"}
          },
          {
            "op": "not",
            "value": {
              "op": "eq",
              "left": {"op": "ref", "root": "facts_of", "path": ["facts", "failed_checks_head"]},
              "right": {"op": "literal", "value": ""}
            }
          },
          {
            "op": "eq",
            "left": {"op": "ref", "root": "facts_of", "path": ["facts", "head"]},
            "right": {"op": "ref", "root": "facts_of", "path": ["facts", "failed_checks_head"]}
          }
        ]
      }
    },
    "edges": {
      "true": "failed_head_hold",
      "false": "publish",
      "unknown": "failed"
    },
    "on_error": null
  },
  "failed_head_hold": {
    "op": {
      "kind": "invoke",
      "command": "work.hold",
      "bind": "failed_head_hold_op",
      "retry": null,
      "input": {
        "op": "object",
        "fields": {
          "task": {"op": "ref", "root": "input", "path": ["task"]},
          "result_id": {"op": "ref", "root": "input", "path": ["result"]},
          "reason": {"op": "literal", "value": "returns_exhausted"}
        }
      }
    },
    "edges": {
      "held": "failed_head_notify"
    },
    "on_error": "failed"
  },
  "failed_head_notify": {
    "op": {
      "kind": "invoke",
      "command": "supervisor.notify",
      "bind": "failed_head_notify_op",
      "retry": null,
      "input": {
        "op": "object",
        "fields": {
          "project": {"op": "ref", "root": "input", "path": ["project"]},
          "identity": {"op": "ref", "root": "input", "path": ["result"]},
          "text": {
            "op": "literal",
            "value": "A task's result is a head whose required checks already failed, so publishing it again cannot change them. It is held for you: rerun the checks of that head, or retry the task."
          }
        }
      }
    },
    "edges": {
      "sent": "held",
      "duplicate": "held"
    },
    "on_error": "failed"
  }
}
```

## Where the pieces are tested

- `checks_head` recorded with `checks_state`: `crates/aq-delivery/src/github/tests/commands/checks.rs`
  (unit) and `playbook_host9::commands::failed_checks_and_the_deadline_each_complete_the_wait_once` (database).
- `failed_checks_head` reported by `work.read` 2.9.0, `""` when there is none,
  and left out of every older catalog's answer:
  `host10::tests::work_read_2_9_0_reports_the_failed_checks_head_or_an_empty_string`
  (unit) and `playbook_host9::delivery::work_read_2_9_0_gives_the_failed_checks_head_or_an_empty_string`
  (database). The 2.8.0 `checks_head`: `host10::tests::work_read_2_8_0_reports_the_head_the_checks_were_evaluated_on`
  and `playbook_host9::delivery::work_read_2_8_0_gives_the_head_the_pull_requests_checks_were_evaluated_on`.
- The steps above, spliced into the published delivery example, pass the
  authoring check (`aq-playbooks check`, `draft::check`) and seal against the
  delivery record's catalog 2.9.0, and the two fixture cases run as written;
  the same reference does not check against 2.8.0, nor does one into
  `pull_request`: `builtin::tests::the_failed_checks_head_rule_checks_on_catalog_2_9_0`
  in `aq-playbook-authoring`.
- Catalog 2.9.0 vendored and pinned, accepted by Host10, the delivery record
  authored on it: `playbook_host9::delivery::host10_accepts_the_delivery_records_catalogs_2_6_0_to_2_9_0`,
  `host10::tests::catalog_2_9_0_fixtures_agree_with_its_registry`,
  `builtin::tests` in `aq-playbook-authoring`, `playbook_environments`,
  `playbook_delivery_document`.
