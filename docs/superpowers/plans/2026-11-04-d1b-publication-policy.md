# D1b: Per-project publication policy and ambiguous-publication reconciliation — plan

Task: `crisp-meadow-36.2`. Base: `8b8ac2b2`. Builds on D1a (`crates/aq-server/src/delivery.rs`).

**Goal:** a revisioned per-project publication policy, a single delivery entry
point (`DeliveryService::deliver`) that D3 calls and D7 wraps, typed refusals,
and aq-git §10.3 reconcile/resolve that never double-publishes.

**Out of scope:** vault credentials and onboarding (D2), playbook adapters (D7),
dashboard and CLI, aq-git branch creation.

## Design

1. **Policy on the project record** (`project_bindings/domain.rs`).
   `ProjectBinding.publication: Option<PublicationPolicy>` with
   `serde(default, skip_serializing_if = "Option::is_none")`. Existing records and
   digests stay byte-identical. The field rides the existing expected-revision
   CAS, the immutable history and the receipts in `project_bindings::mutate`.
   - `PublicationPolicy { mode: pull_request | fast_forward, target, wait_for_checks: Option<ChecksPolicy> }`,
     with `deny_unknown_fields`.
   - `validate()` requires the target to be a full `refs/heads/` branch, and
     allows `wait_for_checks` only in pull_request mode.
   - Allowed-target check: on put, `verify_repository` reads the `allowed_targets`
     from aq-git `repo.show`. A policy target outside that list is refused as
     `Forbidden`.
2. **Wire types** (`delivery/protocol.rs`).
   - `PublishResult::Rejected` gains optional `observed_target` and `observed_at`.
     aq-git sends both for `target_moved`.
   - `ReconcileResult` is `{target_operation_id, resolution?, outcome?, observed_target?, observed_at?, observation_error?}`.
   - `Resolution` is a tagged enum: `already_present` or `resolved`.
   - Validation:
     - the target ID must match the params;
     - the result must be exactly one of a resolution or `outcome == "indeterminate"`;
     - `already_present` must have `caused_by_this_operation == false`;
     - reconcile can never yield `resolved`, and resolve yields only `resolved`
       with the requested disposition.
3. **Reconcile/resolve** (`delivery/reconcile.rs`).
   - Request types:
     - `ReconcileRequest { context, target_operation_id, expected_generation }`;
     - `ResolveRequest { context, target_operation_id, expected_generation, disposition, reason, old_publishers_stopped, evidence }`.
   - Preconditions:
     - the request's own operation ID is new and differs from the target's;
     - the target is a frozen publish intent in the same repository and generation.
   - Resolve also requires `old_publishers_stopped == true`, a non-empty reason
     and non-empty evidence.
   - Each call is frozen through `prepare`/`freeze` under its own operation ID and
     takes the generic non-publish path, so it never touches the publish dispatch
     marker. Its intent and receipt are the immutable audit record.
   - The destination hold (`delivery_destinations` for the target) is released only
     when a validated receipt carries resolution `already_present` (observed
     presence, never `applied`) or `resolved`. A replayed receipt gets the same
     treatment. An `indeterminate` reply keeps the hold.
   - The original publish operation ID is permanently fenced by
     `delivery_publish_dispatches`: re-invoking it returns `Indeterminate` and never
     resends. Any new publication needs a new operation ID and a fresh `expected_old`.
4. **Typed outcomes** (`delivery/policy.rs`).
   - `DeliveryOutcome` is one of:
     - `Published`;
     - `ObservedPresent`;
     - `Refused(PublicationRefusal)`;
     - `Ambiguous { operation_id }`;
     - `DestinationBlocked`;
     - `RecoveryBlocked(RecoveryBlocked)`.
   - `PublicationRefusal { reason: RefusalReason, detail, expected_old, observed_target, observed_at }`.
     The reasons, and what produces each:

     | Reason | Produced by |
     |---|---|
     | `target_moved` | the preflight error, or ok-rejected |
     | `conflicted` | the preflight error, or a local not-ready candidate |
     | `rejected` | any other ok-rejected, carrying aq-git's reason |
     | `target_not_allowed` | `unsupported_feature` |
     | `pull_request_unsupported` | a pull_request policy |

   - `recovery_blocked` is surfaced as typed state.
     `ClearFenceRequest::cli_envelope()` builds the direct-CLI-only
     `recovery.clear-fence` request: `{expected_incarnation, reason, evidence, old_environment_stopped:true}`
     with no repository ID. An operator runs it; it is never automatic and never
     resolves a push.
5. **`deliver(policy, request, github)`**.
   - fast_forward calls `publish` against `policy.target` and maps the receipt to
     a `DeliveryOutcome`.
   - pull_request returns
     `Refused(pull_request_unsupported)` without any aq-git or GitHub call, and
     never degrades to fast-forward.
   - `GitHubAdapter` is a trait (`open_pull_request`, `checks`) with a fake in the
     tests.

## TDD steps

1. Test `ambiguous_push_reconciles_without_double_publish`, in `tests/source_retention/delivery.rs`. Fake aq-git peer:
   - Deliver: the publish replies `operation_indeterminate`, giving outcome `Ambiguous`.
   - A second deliver under a new operation ID gets `DestinationBlocked`, with no socket call.
   - Re-invoking the original operation ID returns `Indeterminate`, with no socket call.
   - `operation_show` returns the generation.
   - Reconcile under new ID A replies indeterminate, and the hold stays.
   - Reconcile under new ID B replies `already_present`, the hold is released, and the outcome is observed presence.
   - Replaying B returns the stored receipt without a socket call.
   - Reconcile with the target's own ID is refused locally.
   - Only one publish request was ever sent.
   - A fresh publish under a new ID and a fresh `expected_old` is admitted.

   Second scenario: resolve with `old_publishers_stopped=false` is refused locally.
   A valid resolve `permit_new_operation` releases the hold.
2. Test `publication_conflict_reports_typed_refusal`:
   - preflight `target_moved` error, `ok:true` rejected `target_moved` with observed OID, `conflicted`, `remote_rejected`, and `unsupported_feature`, each giving the typed `PublicationRefusal` with its OIDs;
   - pull_request policy refuses with zero socket and zero GitHub calls;
   - `recovery_blocked` gives a typed `RecoveryBlocked`.
3. Unit tests: policy validation and serde round-trip (an absent field leaves the
   digest unchanged), and the clear-fence envelope.
4. Implement until they pass. Then run fmt, clippy for `aq-server`, the two named
   acceptance tests, and the existing delivery and project-binding tests.
