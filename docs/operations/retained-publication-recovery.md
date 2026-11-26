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

Keep subsequent identities separate as well: owner evidence also names
`32c10a25-caef-8882-b3f2-20d3f0aa933b`,
`b46211d8-3123-8705-a756-63ba482a75b0`,
`f7f30714-360d-84d3-9506-94aaf6d76f42` and
`72024a5d-7cd8-8af6-a442-9b4d0ac8c0a2`. The latter two have reported
delivery-host-unreachable responses; these establish neither remote rejection
nor acceptance. They do not replace either original operation.
Owner relay additionally names `99d24e83-7f55-885c-ae51-bcf0fb493c58` for
the same retained result with an unreachable report. Preserve it separately.

A separate evidence case relayed by the owner concerns task
`a3c72f06-d1a6-428e-bf67-b0f1c734a8c9` v5, attempt
`0d6984eb-3e1e-59b4-8751-ffb13010a800`, result
`da345d71-4294-4fe8-8ab8-3bdf70fc8f25` at revision
`332b46d7252409a9a4b1d1e0963fca467213f8b1a92598df212449391add4e12`,
retained commit `3bed9a998cd64766f0cdcd0f0da636af21f66da3`. Its original
`3eed25ec-1f83-850e-b983-b0a718b1e9f5` and later
`baf61f21-bce8-8713-a57c-74aed3f3212b` operations must be read under that
task/result's own binding, never through the b57 result above. Reported remote
refusal and empty receipt lists do not establish a shared cause. Repository
binding/generation, original journal and runtime capability still need supported
authenticated verification. This evidence record authorizes no work on that
task or live action against either publication.

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
not a source-version claim. The token must appear in the protocol-v1
`capability_tokens` array (`$.result.capability_tokens` in the wire envelope).
Without `publication_rejection_classification_v1`,
classification is unsupported; a missing historical classification is
`not_recorded`. An old `remote_rejected` diagnostic alone does not establish
which permission or remote rule caused the refusal. The operator must inspect
the original installation and decide any narrowly scoped permission or policy
correction. These tools cannot make that change. Ambiguous acknowledgments
remain indeterminate, with no inferred rejection class.
Unknown phases are returned as null. Future recorded rejection-class values are
reported as nonactionable `unknown`; absent historical fields stay unrecorded.
The frozen target projection contains only its validated repository ID and
binding generation; forge labels and the rest of the stored request stay private.
An original successful journal read includes a bounded `receipt` with verified
target/old/new/time for `applied`, or observed head/time and no causation claim
for `already_present`. The latter may identify a later head containing the
candidate, as attested by the bound original publisher. Recovery preserves that
observed head separately from the retained result commit. Invalid or missing
acknowledgments never create receipts, and returned timestamps are normalized.

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
unobservable remote remains held and uncertain. Contradictory reconciliation
responses containing both an unresolved outcome and a resolution remain uncertain.
A definitive rejection stays
held with its bounded diagnostic and required operator action.

Migration 0185 records immutable dispatch fences, frozen recovery requests and
receipts. It fences existing publication intents before the upgraded driver runs.
New dispatches are reserved only after read-only PR preflight succeeds. Historical refusals remain intact. New publication UUIDs cannot evade
the exact-result dispatch fence. A recovered receipt can re-enter the existing
result-ready/delivery path, where ordinary checks, review and settlement gates
still apply. Recovery does not change source retention, report bytes, budgets,
dependencies, holds or human decisions. There is no forced-publication or
permission-override parameter, and no automatic retry of a refused push.
Only a terminal refusal explicitly recorded by the upgraded host as
`publication_not_dispatched` proves read-only preflight stopped before Git.
That refusal does not consume permission for the first dispatch. An existing
dispatch marker still wins, and historical refusals without that proof stay
fenced. This distinction does not authorize a retry of a dispatched publication.

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
The repair migration was renumbered from 0179 to 0185 after the supplied tree
introduced another 0179 and migrations through 0184. Recheck that numbering
against current main before release; this sandbox has no Git history.
Before using recovery, obtain a separate decision for the exact original
operation and result. Any remote-policy correction or later publication retry
requires its own explicit decision. Existing source leases and review gates
remain prerequisites; a successful source test is not deployment evidence.

The repair's earlier failed-check input is separately bound by a supported
`result.review.show` read: report
`4424f991-19cd-4502-95e9-91b50cd20bf0` at revision
`d9fac451948dfaf937b2f5300051cf2b47c3fc20ec18ab67d3812b20ac10b0ec`,
recorded source head `489acd898966ce1d9b07a61b753748ed55779e32`.
The supported result-read response supplies provenance, without check-job or
assertion fields. A CI/operator read of that revision's bound required-check
failure is still needed; PostgreSQL teardown logs do not identify the failure.
This read does not inspect either original publisher journal or establish
publication acceptance.

The [source validation record](../implementation/retained-publication-recovery-validation.md) lists the exact code fingerprint, commands, outcomes and unrun checks.
