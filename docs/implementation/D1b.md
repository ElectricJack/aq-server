# D1b: per-project publication policy and ambiguous-publication reconciliation

Task `crisp-meadow-36.2`. Base `8b8ac2b2`. Plan: [2026-11-04-d1b-publication-policy.md](../superpowers/plans/2026-11-04-d1b-publication-policy.md). Builds on D1a (`crates/aq-server/src/delivery.rs`).

## What was built

### Publication policy on the project record

`ProjectBinding.publication: Option<PublicationPolicy>` (`project_bindings/domain.rs`).

- `PublicationPolicy { mode: pull_request | fast_forward, target, wait_for_checks? }` uses `deny_unknown_fields`.
- The field is `serde(default, skip_serializing_if = "Option::is_none")`. Records without a policy therefore serialize and digest exactly as before.
- It goes through the existing `project_bindings::mutate` path: the expected-revision CAS, the immutable history and the receipts.
- `ProjectBinding::validate` calls `PublicationPolicy::validate()`, which checks that:
  - the target is a full, well-formed `refs/heads/` branch of at most 1024 bytes;
  - `wait_for_checks` appears only in `pull_request` mode;
  - the `wait_for_checks` timeout is between 1 and 86400 seconds.
- **Allowed targets.** On put, the binding observation (`external.rs`) reads `allowed_targets` from aq-git `repo.show`. A policy target that is not in that list, compared by its exact spelling, is reported as `Forbidden`.

### Delivery entry point (`delivery/policy.rs`)

`DeliveryService::deliver(policy, DeliverRequest{context, candidate_id, expected_old}, &dyn GitHubAdapter)` is the single entry point that D3 calls and D7 wraps. It returns a typed `DeliveryOutcome`:

| Outcome | Produced by |
|---|---|
| `Published{old,new,acknowledged_at}` | aq-git `applied` |
| `ObservedPresent{observed_target,observed_at}` | `already_present`. Recorded as observed presence; `applied` is never fabricated |
| `Refused(PublicationRefusal)` | see the refusal table below |
| `Ambiguous{operation_id}` | `operation_indeterminate`, any uncertain error, a transport loss after dispatch, or replaying a fenced operation ID |
| `DestinationBlocked` | another unresolved publication holds this destination |
| `RecoveryBlocked{operation_id,message,details}` | aq-git `recovery_blocked`. The hold stays |
| `Failed{operation_id,error}` | any other aq-git error |

`PublicationRefusal { operation_id, reason, detail, target, expected_old, observed_target, observed_at }` carries the reason and the observed OIDs. The reasons:

| `RefusalReason` | Source |
|---|---|
| `target_moved` | preflight error `target_moved`, or ok-rejected `target_moved` (with `observed_target`/`observed_at`) |
| `conflicted` | preflight error `conflicted`, or a candidate that is not `ready` locally (no aq-git call) |
| `rejected` | any other ok-rejected reason, such as `remote_rejected`. `detail` carries aq-git's reason |
| `target_not_allowed` | `unsupported_feature`: the target is not in `allowed_targets` |
| `pull_request_unsupported` | a `pull_request` policy |

### Pull-request mode

`GitHubAdapter` (`open_pull_request`, `checks`) is a trait; the tests use a fake.

aq-git 0.2.0 publishes only to existing allowed branches and cannot create the PR head branch. So `pull_request` mode returns `Refused(pull_request_unsupported)` before it freezes any intent or calls aq-git or GitHub. It never degrades to fast-forward.

### Reconcile and resolve (`delivery/reconcile.rs`, aq-git §10.3)

**Reconcile.** `reconcile(ReconcileRequest{context, target_operation_id, expected_generation})` sends `operation.reconcile` with params `{target_operation_id, expected_generation}`.
- It runs under its own new operation ID. Reusing the target's ID is refused locally.
- The target must be a frozen publish intent of the same repository and binding generation.
- The caller takes the generation from `operation_show`.

**Resolve.** `resolve(ResolveRequest{..., disposition: abandon | permit_new_operation, reason, old_publishers_stopped, evidence})` sends `operation.resolve`.
- It is refused locally unless `old_publishers_stopped == true`, the reason is non-empty and the evidence is a non-empty string or object.
- The frozen intent (with its inputs, reason and evidence) and the receipt are the immutable audit record.

**Shared mechanics.** Both calls are frozen through D1a's `prepare`/`freeze` under their own ID and go through the generic non-publish path. They never touch the publish dispatch fence, so nothing is ever resent.

**Hold release.** The destination hold is released only when a validated receipt carries resolution `already_present` (from reconcile, with `caused_by_this_operation == false`) or `resolved` (from resolve, echoing the requested disposition and reason). A replayed receipt releases the same way. An `indeterminate` reply keeps the hold and the ambiguity.

**The original operation ID stays fenced.** `delivery_publish_dispatches` keeps it fenced permanently: delivering it again returns `Ambiguous` and sends nothing. A later publication needs a new operation ID and a fresh `expected_old`.

**Wire validation (`protocol.rs`).**
- `ReconcileResult` must echo the target ID.
- It must be exactly one of a resolution or `outcome:"indeterminate"`.
- `indeterminate` and `already_present` are valid only for reconcile; `resolved` only for resolve.
- OIDs and RFC 3339 times are checked.
- `PublishResult::Rejected` gains optional `observed_target` and `observed_at`.

### Clearing a recovery fence

`ClearFenceRequest::cli_envelope()` builds the `recovery.clear-fence` request, which aq-git accepts only over the direct CLI (it refuses the request on the socket):

```
{expected_incarnation, reason, evidence, old_environment_stopped:true}
```

The envelope has no `repository_id` or binding generation. An operator runs it. It is never automatic and never resolves a push.

### Schema

Migration `0077_delivery_reconciliation.sql` widens the `delivery_intents.operation` check to allow `operation.reconcile` and `operation.resolve`.

Main had two `0075` files after #70 and #73 merged, so the migration catalog failed to build. This branch moves the later-merged `0075_supervisor_owner_attachments.sql` (#73) to `0076`, with meta `76`. `0075_result_media_clips.sql` (#70, merged first) keeps `0075`.

## Acceptance evidence

Commands run in this worktree against local Postgres (`DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres`). Each goes through `aq test docs/implementation/D1b-checks/run_cargo.py`, with `CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=2`. Logs are in `docs/implementation/D1b-checks/`.

**Acceptance and unit tests** (`acceptance.log`):

```
cargo test --locked --workspace -- publication_conflict_reports_typed_refusal \
  ambiguous_push_reconciles_without_double_publish policy_validation_and_round_trip \
  clear_fence_envelope_is_direct_cli_only publication_target_must_be_an_allowed_target \
  state_format_is_the_latest_migration
```

```
test publication::ambiguous_push_reconciles_without_double_publish ... ok
test publication::publication_conflict_reports_typed_refusal ... ok
test delivery::policy::tests::clear_fence_envelope_is_direct_cli_only ... ok
test delivery::policy::tests::policy_validation_and_round_trip ... ok
test project_bindings::external::tests::publication_target_must_be_an_allowed_target ... ok
test store::tests::state_format_is_the_latest_migration ... ok
```

**`ambiguous_push_reconciles_without_double_publish`** proves the ambiguity path end to end:
- A fast-forward delivery whose aq-git reply is `operation_indeterminate` returns `Ambiguous` and keeps the destination hold.
- With no aq-git socket bound, a new delivery to the same destination returns `DestinationBlocked` and replaying the original ID returns `Ambiguous`. Neither is a transport error, so nothing was resent.
- Reconcile is refused locally when it reuses the target's operation ID.
- Reconcile under a new ID sends `{target_operation_id, expected_generation:4}`, with the generation taken from `operation_show`. An `indeterminate` reply keeps the hold, and deliveries stay `DestinationBlocked`.
- A second reconcile under a new ID gets `already_present` (`caused_by_this_operation:false`), which releases the hold. Replaying it exactly returns the stored receipt. The original ID is still `Ambiguous`.
- A later publication with a new ID and a fresh `expected_old` is sent with those values and reports `ObservedPresent`.
- Resolve is refused without `old_publishers_stopped`. With it, a `resolved` reply releases a second ambiguous hold on `refs/heads/release`.
- Exactly one `delivery_publish_dispatches` row exists for the original operation, and there are zero GitHub calls.

**`publication_conflict_reports_typed_refusal`** covers each refusal:
- `target_moved`, both as an error and as an ok-rejected reply that carries `observed_target`.
- `conflicted`.
- `rejected` with `detail=remote_rejected`.
- `unsupported_feature`, reported as `target_not_allowed`.

In every refusal case the test checks the `expected_old` echo and that no hold remains. It also checks that:
- a locally conflicted candidate is refused as `conflicted` without any aq-git call;
- `pull_request` mode is refused as `pull_request_unsupported`, freezes no intent and makes zero GitHub calls;
- `recovery_blocked` returns `RecoveryBlocked` and the hold stays.

**Regression** (`regression.log`): `cargo test --locked -p aq-server -- delivery project_binding`. Every matching unit and integration test passes (D1a delivery, project bindings, actual-owner setup), with 0 failures.

**Lint:**
- `rustfmt --edition 2024 --check` on every changed `.rs` file reports no diffs. `cargo fmt --all` cannot run in a worktree, because the vendored crate's workspace detection fails.
- `cargo clippy --locked -p aq-server --tests` reports no warnings or errors.

## Notes and follow-ups

- If aq-git answers `recovery_blocked` to a publish, the destination hold stays: it carries no `operation_state`, so it is not proof of a refusal. An operator clears the aq-git fence with `recovery.clear-fence`, then settles the hold with reconcile or resolve under a new ID.
- Pull-request mode needs aq-git branch creation. That is out of scope here; see the cross-repo follow-up comment on the task.
- There are no dashboard or CLI surfaces for policy, reconcile or resolve. Those belong to G-series and F7.
