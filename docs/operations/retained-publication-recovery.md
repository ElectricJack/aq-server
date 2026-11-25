# Inspect and recover an exact retained-result publication

This procedure preserves the worker's result and every original publication
identity. It does not authorize a live reconciliation, a retry, a permission
change, acceptance, release, merge or deployment. An operator must separately
authorize use of the recovery command after ordinary review and release checks.
No command in this procedure sends `publish` or `operation.resolve`.

The commands are available in a server built with `delivery_playbook`. An owner,
or a supervisor with the existing owner bridge (`supervisor_full_access`), may
inspect/reconcile. A project supervisor is restricted to its own project path.
Agent grants are unchanged. The configured `source_retention.git_socket` must
reach the **original** aq-git installation; a supervisor-host `operation.show`
is a different journal. Its `not_found` says nothing about the push.

## Read the original request and journal

First read `result.review.show` for the exact report digest. That authenticated
read confirms the project's retained repository, base and commit; it does not
prove publication or acceptance. For the reported incident those are:

- Project: `a22afccc-d61a-5f77-aef8-9fd73311c68a`.
- Repository: `97a459e3-e35f-5af0-85c0-1c095b50519c`.
- Task: `b57bd9b1-983d-43ef-abbf-2798cb829cfc`.
- Attempt: `a80becbb-b786-5e71-adcc-d248ea83e969`.
- Result: `68d19730-4db8-4668-855b-d3e9572d5050`.
- Revision: `f4b26ac1956b58997c0faca587f675453b20c9fb127da61dbf701b038f2b7b19`.
- Retained commit: `ac8b015d6dfaaa0aa7850d2e7ff13a2d20285cdd`.

Use the following read separately for each original operation, including
`dfd8cb53-9b5e-833c-bfe8-f32b36408c66` and
`de4542bf-b430-8e57-a85e-9eb6f957679d`:

```sh
aq run result.publication.show \
  --param project_id=a22afccc-d61a-5f77-aef8-9fd73311c68a \
  --param repository_id=97a459e3-e35f-5af0-85c0-1c095b50519c \
  --param report_id=68d19730-4db8-4668-855b-d3e9572d5050 \
  --param revision=f4b26ac1956b58997c0faca587f675453b20c9fb127da61dbf701b038f2b7b19 \
  --param publication_operation_id=dfd8cb53-9b5e-833c-bfe8-f32b36408c66 --json
```

The list returns at most 25 original/subsequent operations. Follow `next_after`
with `--param after=UUID` until it is null. UUID pagination does not reorder or
replace historical identities. Selecting one operation additionally reads its
private aq-git journal with `operation.show {target_operation_id}`. The bridge
checks the payload digest, repository and binding generation, source ref and
expected source commit, target ref and original remote-head lease. A mismatch
refuses disclosure of the publisher record. Only bounded protocol fields and
allowlisted diagnostics are returned, never raw journal state, stderr, remote
messages, hooks, credentials or arbitrary metadata.

`classification_capability` reports the actual runtime capabilities response,
not a source-version claim. Without `publication_rejection_classification_v1`,
classification is unsupported; a missing historical classification is
`not_recorded`. An old `remote_rejected` diagnostic alone does not establish
which permission or remote rule caused the refusal. The operator must inspect
the original installation and decide any narrowly scoped permission or policy
correction. These tools cannot make that change. Ambiguous acknowledgments
remain indeterminate, with no inferred rejection class.

## Recover a receipt after separate operator authorization

Use `result.publication.reconcile` with the same project, repository, report
and revision, `publication_operation_id` set to the original publication, and
a fresh `operation_id` for the recovery. Preserve all request bytes and that
recovery UUID on uncertain transport. Inspection of the selected original also
returns its latest retained recovery receipt. Repeating the recovery request
returns its immutable receipt; changing its identity conflicts. Use a new
recovery ID for a later observation after an earlier held observation.

The server freezes the original journal generation before calling
`operation.reconcile` with the original repository/binding and
`{target_operation_id, expected_generation}`. Even if the response or database
receipt is lost, the request and generation survive. Only pending
`dispatched`/`indeterminate` publications qualify for reconciliation. It is an
observe/fetch operation; the original publication is never sent again.
An already validated successful original receipt is recovered directly.
`already_present` is recorded without a causation claim. A changed, absent or
unobservable remote remains held and uncertain. A definitive rejection stays
held with its bounded diagnostic and required operator action.

Migration 0179 records immutable dispatch fences, frozen recovery requests and
receipts. It fences existing publication intents before the upgraded driver runs.
New dispatches are reserved only after read-only PR preflight succeeds. Historical refusals remain intact. New publication UUIDs cannot evade
the exact-result dispatch fence. A recovered receipt can re-enter the existing
result-ready/delivery path, where ordinary checks, review and settlement gates
still apply. Recovery does not change source retention, report bytes, budgets,
dependencies, holds or human decisions. There is no forced-publication or
permission-override parameter, and no automatic retry of a refused push.

## Evidence and release prerequisites

The source-only sandbox authenticated the exact result/retained commit above;
the report still had empty `source_receipt_ids`. The deployed supervisor-host
read of operation `dfd8cb53` returned `not_found`. The sandbox did not have a
supported private-journal bridge, so it could not determine the original remote
rejection class, confirm runtime capability, or prove publication acceptance.
No live reconciliation was run. Cross-project supervisor coordination was
refused by the scoped mailbox; the protocol/evidence request was forwarded to
the global supervisor. Later mailbox reads became unauthenticated, which is
not authority to change credentials.

Before release, the operator must approve the reviewed server change/migration
and verify access to the original private socket and its runtime capability.
Before using recovery, obtain a separate decision for the exact original
operation and result. Any remote-policy correction or later publication retry
requires its own explicit decision. Existing source leases and review gates
remain prerequisites; a successful source test is not deployment evidence.

The [source validation record](../implementation/retained-publication-recovery-validation.md) lists the exact code fingerprint, commands, outcomes and unrun checks.
