# First-pilot combined review and settlement HTTP ports

Production activation and the later combined-decision admission ports are
documented in [I0 settlement activation](I0-settlement-path.md). The qualification
record below describes the original adapter delivery.

This is a thin protected adapter over the existing combined-review and settlement owners. It adds no schema, decision admission, automatic answer, scheduler or eligibility logic. A real human must choose the answer; synthetic answers in component tests are not physical execution or human-review qualification.

The existing optional `reviews::admin_routes` injection mounts these routes beneath the daemon's current identity, origin/CSRF, admission, maintenance and response-hardening layers. Default constructors without a configured reviews owner keep every new route absent. Handlers pass the complete authenticated `store::User` from `RequireOwner`; they never substitute a UUID, worker or agent credential. Settlement uses the injected reviews owner's actual AppState and Service.

| Method and path | Body | Actual owner call |
|---|---|---|
| `GET /api/reviews/combined/{decision_id}` | none | `inspect_combined_as(&User, decision_id)` |
| `POST /api/reviews/combined/{decision_id}/resolve` | `{"expected_subject": ArtifactRef, "answer": ResolveRequest}` | `resolve_combined_as(&User, &answer, &expected_subject)` |
| `POST /api/settlement/evaluations` | existing `EvaluateRequest` | `settlement::Service::evaluate(&User, &request)` |
| `GET /api/settlement/evaluations/{operation_id}` | none | `settlement::Service::lookup(&User, operation_id)` |
| `POST /api/settlement/completions/{operation_id}/reconcile` | exactly `{}` | `settlement::Service::complete(&User, operation_id)` |

New resolve/evaluation envelopes, their subject references and resolve answers require JSON objects and reject valid positional arrays. The feature-local evaluation body passes all six existing fields unchanged to the owner; it supplies no default or restamped value. The resolve envelope rejects unknown/duplicate fields, requires path ID equal to `answer.decision_id`, and requires the complete expected subject reference. The owner checks the combined profile and current retained subject, principal, decision revision and exact operation identity, including replay. Existing code-only endpoints and inbox keep their original wire and exclude combined subjects.

All mutation bodies use `ApiJson` and the existing 16KiB review route limit. Mutations require `X-AQ-Request: 1`; a supplied Origin must equal the configured public origin. Reconcile has a closed empty body to prevent introducing an unbound choice or dispatch parameter. It can send the already admitted Graph completion effect; GET never invokes it. Lookup returns the immutable evaluation receipt and does not refresh eligibility or dispatch work. Complete/reconcile uses the existing finite original operation, current actor and configured service identity; it does not invent a new completion ID or bypass the actual owner fences.

Responses are existing owner values: combined inspection JSON, `DecisionReceipt`, `EvaluationReceipt`, or `CompletionStatus`. Existing ApiError/status conventions apply: 400 malformed/path mismatch, 413 body bound, 403 unauthorized/ineligible/profile or expected-review-subject refusal, 409 evaluation subject/revision/operation conflict, 404 absent route or operation, and 503 unavailable current evidence/service. A failed mutation may have a possible effect; clients retain original IDs/body and use the owner's exact replay/lookup semantics, not a new approval or completion.

A valid current combined question without a recorded answer returns 503 after the owner locks and verifies the retained B1 subject. This creates no evaluation, completion or release work. After the human answers, the same previously refused evaluation intent can be submitted unchanged. This classification applies only to the new combined profile; legacy callbacks keep their existing behavior.

Focused qualification reuses `profiled_results/settlement_owner39` actual SQL owner fixtures with explicitly synthetic launch/Git/Graph peers. Tests cover default absence, Owner/Viewer/identity rebinding, origin and closed bodies, old/new profile separation, expected-subject mismatch, explicit answer replay, immutable evaluation lookup and exact completion recovery after a lost Graph response. No provider, canonical database, production source or external human message is involved. Parent execution and human-approval gates remain open until separately qualified.

## Qualification

The product/source freeze is `54c113681ca843349dd8a6c17a168416b965ceea`, based on `245f6ed` and merged with Root's qualified `0d1cdc8`. The pending-review owner correction is Decisions' `1cd5f57`, imported as `f960373`. Decisions independently accepted the thin routes, the object-only root/nested envelopes (`16e75f3`) and the pending-question HTTP vector (`54c1136`). The HTTP author separately reviewed the new-profile pending classification and full-principal error fence; no legacy callback behavior was changed.

The final focused gate uses the existing SERVER cache, two Cargo jobs, no incremental/debug payload, and offline/locked dependencies. It uses disposable UUID test databases on the shared local test service; it does not migrate the canonical database or start an agent/provider. Commands and captured output are retained in [the evidence directory](evidence/human-settlement-http-54c1136/manifest.json).

- Exact pending-review owner negative: 1 passed in 1.45 s after a 50.20 s compile.
- Focused library and `profiled_results` Clippy with warnings denied: passed in 26.98 s.
- Protected HTTP route group: 5 passed in 1.76 s, including strict root/nested JSON objects, optional mounting, machine-listener exclusion, Owner/Viewer/current-identity fences, origin/body bounds, old/new profile separation, answer replay, read-only evaluation lookup, and original-effect reconciliation.

Tests deliberately label their review choices and launch/Git/Graph peers as synthetic. These results qualify the protected adapter and its actual persisted owner calls. They do not record a real human answer, qualify the physical agent execution, or complete the parent execution milestone. Runtime activation and the user's human choice remain separate.
