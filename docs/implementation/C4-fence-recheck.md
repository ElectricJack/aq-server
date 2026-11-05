# C4 follow-up: placement re-check at the admission fences

Task: `calm-delta-18.6.2`. The branch was started stacked on `aq/calm-delta-18.6` (C4), which has since merged to `main` together with `calm-delta-18.6.1` (H2 node spend). The final `git merge origin/main` adopts main's migration numbering (`0055_environment_templates.sql`, state format 55).

## What changed
- `aq_selection::recheck(&Placement, &ConcreteChoice)` runs the selection placement checks against the frozen choice and returns the same typed reason (`crates/aq-selection/src/placement.rs`).
  - `check` and `recheck` share `check_parts`, so selection and fences cannot drift apart.
- `profiled_execution::placement::derive(conn, …, Derivation)` re-derives placement facts on a caller's connection.
  - It reads `delegated_node_boots`, `profile_catalog_environment_templates` and `execution_attempts` on that connection.
  - Node spend for the spend ceilings comes from the H2 observation ledger (`Ledger`, `observation_pool`), as at selection. That ledger lives in a separate pool, outside the fence transaction. An unreadable ledger refuses with `node_spend_unaccounted`.
  - Selection uses `Derivation::SELECTION`. `facts`/`facts_in` are unchanged.
- `fence_locked` runs inside `fence_retained_decision`, inside the SERIALIZABLE admission transaction, after `fence_snapshot`. That covers:
  - `fence_for_offer` (offer publication);
  - `fence_for_preparation` (preparation, accepted preparation, initial grant);
  - `fence_for_grant_replay`.
- Fence refusals are 409 `Conflict` with the selection reason code, checked in this order:
  1. `node_concurrency_ceiling_reached`
  2. `node_spend_ceiling_reached`
  3. `node_spend_unaccounted`
  4. `required_backend_capability_unsupported`
  5. `environment_template_unresolved`
  6. `environment_template_digest_mismatch`
  7. `template_not_advertised`
  8. `template_missing_selected_harness_version`
- If the node gained or lost a sandbox registration since selection (selected `Some`/current `None` or the reverse), the fence refuses with `placement_registration_changed`. Host-native decisions (`None`/`None`) are unchanged.

## Decisions
- **Preference hold is selection-only.** The fence derives with `peers: false`, so `preferred_peer` is always `None`. Once a node holds an offer, a preferred peer coming online does not take it away, and skipping the peer scan keeps the fence's serializable read set narrow.
- **Owned attempt is excluded from the ceiling count.** `execution_attempts.holds_task` defaults to true, so an owned attempt is already counted. The preparation and grant fences pass `exclude_attempt = Some(attempt_id)`. The offer fence runs before the attempt row exists and passes `None`.
- **No `(environment, template)` index.** No query filters by that pair. Pins are read by the `catalog_digest` primary-key prefix (at most 64 rows), and peers are read by `node_id`. A migration would also bump the state format again for no read-path benefit.
- **Peer-scan bound.** There is no metrics crate in the server. When the selection peer scan reaches `MAX_PEERS` (256), the server emits `tracing::warn!` with target `aq_server::placement` (`peer_scan_truncated`).

## Tests
- `aq-selection`: `fence_recheck_refuses_frozen_choice_with_selection_reasons` checks parity with selection for each of the eight reason codes above.
- `aq-server`: `profiled_execution::placement::tests::fence_rechecks_ceiling_and_template_after_selection` covers:
  - a concurrency ceiling reached after selection, and owned-attempt exclusion;
  - a template re-pin (digest mismatch) and a dropped pin (unresolved);
  - a registration that stops advertising the template;
  - registration appearing or vanishing;
  - the hold being absent at the fence;
  - the peer-scan bound.
