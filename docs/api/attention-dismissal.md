# Needs-you attention list and dismissal

Implemented server contract for task 602c3994, based on the proposed dashboard
contract at aq-dashboard commit 11349ba. Dashboard controls are a separate task.

`GET /api/v1/bootstrap` advertises `attention.dismiss`. It is enabled for owners.
Owner authorization is checked against current server state on **every** call,
including repeats and requests for subjects that are already gone. Other
principals receive 403; a supplied project UUID grants no access.

`POST /api/v1/attention/{attention_id}/dismiss`

```json
{"expected_revision":"12"}
```

Use the exact string in the attention item's `subject.revision` (numeric owner
revision or `sha256:` digest), and URL-encode the attention ID as one path segment.
Internal notifications expose their source row revision even when their link
opens the affected task. A changed internal notification gets a new revision.

A successful response is HTTP 200:

```json
{"attention_id":"internal_work:00000000-0000-0000-0000-000000000001","expected_revision":"12","dismissed":true,"dismissed_at_ms":2000}
```

Repeating a dismissal returns the **same record**, including the original
`dismissed_at_ms`. A current subject with a different revision returns 409
`attention_revision_changed`, even if the requested revision was previously
dismissed. An already-gone subject succeeds. Malformed IDs or revision bodies
are rejected with 422. Missing `expected_revision` is invalid.

Decisions, questions, and internal work directed at decisions or questions
return 409 `attention_not_dismissible`. So do notifications whose subject is a
pending review gate (including failed results and tasks with pending reviews).
Pending requests stay protected until their owner resolves them; passage of
an expiry timestamp alone does not clear a gate. Blocked tasks currently have
no mounted overview owner and are conservatively refused. Failed results,
internal notifications, offline workers and uncertain-operation notifications
can otherwise be dismissed. Dismissing an uncertain operation only hides its
notification; use its original operation ID to inspect the operation's status.

Internal notifications remain visible when their affected record has a pending
review, including notifications that link to the internal run rather than the
affected record. This protection also overrides an earlier saved dismissal.

Dismissal only writes the per-principal dismissal ledger. It never approves,
releases, cancels, resolves, retries, or changes the underlying subject. Dismissed
revisions are excluded from both `attention[]` and `counts.needs_you` in
`GET /api/v1/overview`, and from `GET /api/v1/attention`, across refresh and server
restart. Unknown totals remain unknown when a source is unavailable. A changed
revision or newly pending gate is visible again. Dismissals have no eviction.
The already supported attention list accepts `cursor`, `limit`, `kind`, and
`scope`; it pages the overview's bounded source sample, not unseen owner records.

Migrations 0187–0188 follow main's migration 0186. The retained checkout lacked
main's migrations 0179–0186; these are restored unchanged from aq-server commit
`3d236e533d7b7ea31b9417d9f5942c4dc1010181` to keep the catalog contiguous.
Migration 0187 creates the dismissal ledger; 0188 adds revision keys. Existing
dismissals are pinned to their current retained source revision.
Unmatched legacy rows remain retained but cannot hide a newly revisioned subject.
