# H2: Production usage accounting and the pricing reference (TDD plan)

Task: `clear-crest-61.2`. Scope: the two named acceptance tests and the wiring
they prove. Out of scope: H1 runner readers and store (aq-worker), G4 provider
quota, G10 dashboard, H3 stall ladder, and enforcing hard limits beyond refusing
ones the harness cannot enforce.

## Acceptance tests (must exist and pass with `cargo test --locked --workspace <name>`)

| Test | Location | Proves |
|---|---|---|
| `attempt_usage_feeds_ledger_and_budget` | `crates/aq-server/tests/observations.rs` | A catalog-derived source is registered from the production attempt path. An H1 batch through the batch route reaches the ledger, and `GET /api/observations/sessions/{attempt}/usage` reports a `NonInvoiceEstimate{TokenRates}` priced by the pinned rate card. A rate-card correction (revision 2) re-prices once without double billing. A replay does not recount. A retry attempt in the same lineage shares the lineage spend and does not reset it. Reaching the project soft ceiling records exactly one policy event. A hard-ceiling requirement stays refused (`hard_budget_provider` is None). |
| `flat_subscription_usage_is_labelled_not_costed` | `crates/aq-server/tests/observations.rs` | With a `subscription_login` credential, token counters are recorded, the fact pins no rate card, and the usage costs contain no token-rate estimate. Cost is `Unknown{NotApplicable}` with a subscription-basis completeness label. |

## A. Rate card book (`crates/aq-accounting/src/pricing.rs`)

1. RED: a unit test `reported_zero_is_distinct_from_unknown` together with the
   correction-chain checks.
2. GREEN:
   - `RateCardRevision{revision, corrects: Option<ArtifactRef>, card}`.
   - `RateCardBook::new` validates every card, requires unique references, and
     requires consecutive revisions per (provider, model) starting at 1. Each
     revision corrects its predecessor with the same layout and currency.
   - `current(provider, model)` returns the newest card. Ingest pins it.
   - `effective(pinned)` returns the newest revision in the pinned card's family.
   - `RateCard::price` is `estimate_fact` minus the exact-reference check. The
     layout must still match.
   - `summarize(&[(Option<EstimateBasis>, Fact)], &book) -> Summary{costs, spent, currency, complete, reason}`:
     - One `ProviderReported` component per currency, including a reported zero.
     - One `NonInvoiceEstimate{TokenRates}` per (effective card, currency).
     - Distinct `Unknown` reasons for everything else.
     - Subscription or local billing yields `Unknown{NotApplicable}` with a
       subscription or local-allocation reason, never a token-rate charge.
     - The result has at most 8 components and a lower-bound spend.

## B. Trusted accounting catalog (`intelligence/config.rs`, `observations/catalog.rs`)

1. `IntelligenceConfig.accounting: AccountingCatalog{sources: [SourcePolicy{provider, harness, reference, layout, parsers}], rate_cards: [RateCardRevision]}`.
   - It is validated in `validate()`. It is kept out of the registry digest, so
     existing registries keep their digest.
2. `OwnerSource.billing: Option<EstimateBasis>`, serde-default and
   skip-if-none, so the digests of existing rows are unchanged.
3. `catalog::owner_source(registry, binding, selection)`:
   - The source policy comes from the entry matching (provider, harness).
   - `account_scope` and `call_namespace` are digests of (provider, account_reference).
   - `source_id` is derived deterministically from (attempt, generation, entry).
   - `native_session_key` is `aq.attempt/<attempt>/<generation>`.
   - `billing` comes from the credential kind: ApiKey gives TokenRates;
     SubscriptionLogin gives SubscriptionAllocation.
4. `catalog::register_attempt(state, attempt)` loads the attempt's offer and
   selection and calls `observations::register_source`. It is idempotent.
5. Production callers:
   - `started::record`, after commit, best effort.
   - The batch route, for each attempt in the batch, before ingest.

## C. Ingest and read

1. Ingest pins `pricing_reference = book.current(provider, model)` only when
   `source.billing == Some(TokenRates)`.
2. `inspection::usage` loads the attempt's call heads and the billing of the
   owner sources, then summarizes:
   - `costs` comes from the summary.
   - Completeness is `CompleteForPolicy` only when the summary is complete and
     the projection has no incomplete or unresolved calls. Otherwise it is
     `Incomplete{reason}`, or `Unknown` when there are no calls.

## D. Lineage budget (`observations/budget.rs`, resolver)

1. `budget::lineage(state, lineage, ceilings)` sums the summary over every head
   in the lineage, across all attempts. Retries and wakes share the lineage, so
   nothing is reset. Replayed calls are deduplicated by call key, so nothing is
   recounted.
2. Soft ceiling: a project `cost_limit_microunits` without
   `require_hard_cost_enforcement`. When the spend reaches it, an
   `accounting_policy_events` row is inserted idempotently.
3. The resolver fills `BudgetSnapshot.{spent_microunits, currency, accounting, accounting_source}`
   from the lineage budget when a ledger projection exists. `hard_budget_provider`
   stays None, so `hard_cost_enforcement_unqualified` still refuses hard
   requirements (0004 §2.2).
4. Migration 0054 adds `accounting_policy_events` (immutable) and a partial
   owner index on `observation_call_heads`. The state format moves to 54 in
   `store.rs`, `lib.rs` and the Dockerfile.

## Verification

- `cargo test --locked --workspace attempt_usage_feeds_ledger_and_budget`
- `cargo test --locked --workspace flat_subscription_usage_is_labelled_not_costed`
- `cargo test --locked -p aq-accounting`
- `cargo test --locked -p aq-server --test observations`
- `cargo fmt --check` and clippy on the touched crates.
