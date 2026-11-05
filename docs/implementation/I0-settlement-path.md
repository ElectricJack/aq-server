# I0 settlement activation (keen-impact-58)

The server can mount the protected review and settlement owner without a
playbook engine. Add this to the qualification server configuration and restart
the server built from this change:

```toml
[reviews.protection]
active_key_id = "reviews-v1"
keys = [{ id = "reviews-v1", file = "/absolute/private/path/reviews-v1.key" }]
```

The key file contains exactly 32 raw bytes, is owned by the server user, and has
no group or other permissions. Retain it across restarts and retain old keys
when rotating: decisions and settlement receipts are encrypted with these keys.
Missing or unsafe configured keys fail configuration loading before listeners
start. If playbooks already supply the review service, that service and its
configured keyring remain authoritative; standalone settings are unnecessary.
Without either configured owner the routes remain absent.

All these routes use the human admin listener, current Owner authentication,
the existing origin check, and `X-AQ-Request: 1` for mutations. Worker credentials
cannot admit or answer a human decision.

1. `POST /api/reviews/combined/prepare` with JSON object fields `report_id`,
   `source_operation_id`, and `external` (the existing `ExternalDeclaration`).
   This returns the actual `CombinedSubject` after retained source and AQ effect
   reads. Compute its artifact reference with `CombinedSubject::artifact()`.
2. `POST /api/reviews/combined` with those same fields plus `decision` (the
   existing `DecisionRequest`, naming that exact artifact, policy, eligible
   Owners, expiry, and stable operation/decision IDs). The server rebuilds the
   actual subject and invokes `admit_combined`; it does not trust supplied
   subject bytes as preparation proof. Exact admission replay is supported.
3. Inspect and explicitly answer the question using the existing
   `GET /api/reviews/combined/{decision_id}` and
   `POST /api/reviews/combined/{decision_id}/resolve` ports. A real human supplies
   the answer after inspecting the exact source, AQ effects and external
   declaration.
4. Submit the existing `EvaluateRequest` to `POST /api/settlement/evaluations`.
   Read its immutable receipt at `GET /api/settlement/evaluations/{operation_id}`.
5. Reconcile the returned completion operation with
   `POST /api/settlement/completions/{completion_operation_id}/reconcile`, body
   `{}`. Only confirmed completion releases the execution hold. Preserve exact
   IDs and bodies when recovering an uncertain response.

The existing [HTTP contract](GATE-04-human-settlement-http.md) describes the
resolution and settlement bodies. New admission envelopes reject unknown fields
and positional root JSON arrays. Existing source, current principal, eligibility,
report-head, effect reconciliation and immutable replay fences still apply.

Authenticated relay uploads now discard disposable output when no watch/capture
buffer exists or its lease expired. They allocate no buffer and still validate
the peer boot, current terminal identity and authority before discarding. Active
watchers retain their snapshot/sequence checks. Internal relay failures retain
HTTP 500 with safe endpoint codes `terminal_upload_failed`,
`terminal_poll_failed`, `terminal_output_poll_failed`, or
`terminal_control_upload_failed`. Existing authorization and domain errors keep
their original classifications; diagnostics never include output or SQL text.

A changed retained definition with the same ID/version returns HTTP 409 and
`admission_definition_identity_conflict`. Publish a new definition version;
do not mutate existing retained bytes. Native producer polling preserves the
pinned `ControlReply::Failure` / `conflict` body and names this reason in the
`aq-error-code` response header and safe server warning. Admin errors name it
in the usual JSON code field. The database trigger remains the final fence.

Run focused component qualification with `DATABASE_URL` from `scripts/test-db`:

```sh
aq test scripts/test-i0-settlement.py -q
```

The wrapper retains Cargo output under `.aq/tmp/i0-settlement-tests/`. It covers
standalone key configuration, production route mounting, exact combined admission
and replay, pending decision refusal, explicit synthetic approval, evaluation,
confirmed reconciliation and single hold release, missing-watcher uploads,
preserved stale-peer refusal, injected relay database failures, and immutable
definition identity conflicts. Source/Graph peers and human choices in these
tests are synthetic. A successful calm-stone-85 qualification run and an actual
human answer on aq-e0-qual remain separate acceptance evidence.

The focused gate passed 56 Rust tests (1 native diagnostic, 2 runtime,
15 configuration, 5 artifact retention, 6 output relay, 22 terminal persistence,
and 5 settlement HTTP), using two Cargo jobs and one Rust test thread against
disposable PostgreSQL databases. The wrapper passed in 611.53 seconds; logs are
in `.aq/tmp/i0-settlement-run2/` and `.aq/tmp/i0-pytest-run2.log`. Direct Rust
format and Git whitespace checks also passed. The initial compile-only failure
was a missing new field in a test configuration initializer, corrected before
this gate; it supplies no passing evidence.
