# aq-accounting

A pure accounting proposal kernel. It owns no database, authentication, source
reader or provider API. The central adapter verifies account/conversation
namespaces, parser policy, exact lineage and prior facts, then applies a decision
with the event cursor, immutable ledger entry and projection in one transaction.
A structurally valid artifact reference alone does not establish that ownership.

Call keys use stable account/native namespaces and native call IDs. Parser
versions and model display names do not create new call identities. Cumulative
keys additionally bind source/counter generations and require an owner-verified
baseline for the exact counter key. Counter decrease or attribution drift is
unresolved evidence, not new spending. Equal-priority conflicts, unknown/stale
corrections and foreign-lineage changes remain explicit. Imported known history
adds no charge. Higher-priority sources and exact corrections replace one
canonical contribution while appending an audit fact.

`projection::Projection` is constant-size and replaces the old canonical
contribution atomically. It retains known subtotals separately from unknown
contributions. No observations is unknown; a reported zero call is zero.
Unresolved observations make the projection incomplete. This is not a hard
budget-enforcement boundary or proof of parser/native accounting completeness.

`Layout` defines the verified inclusion relationships among input/output/cache
and reasoning categories. Unique totals use those relationships. Rate cards
contain explicit disjoint billing rules that cover every semantic category,
including explicit zero rates where appropriate. Pricing uses integer
microunits, rounds once per canonical contribution, and preserves unknown cost
plus a labelled known lower bound when required counts are absent.

Use `RateCard::estimate_fact` for production projections: it checks the
canonical fact's pinned rate card and category policy. Estimates are explicitly
non-invoice costs; unverified transcript/provider charge fields never become
trusted prices. Mixed currencies are not converted. A native model-attribution
correction that cannot use the pinned card remains unpriced until an owning
repricing operation is provided.

The crate returns proposed facts and projection changes; it does not implement
persistence, central source registration, human permissions, transcript transfer
or retention. Those are AQ22's remaining server work. All tests are synthetic
accounting evidence and perform no network or provider calls.

`vm_time` separately models Boat sandbox sizes, pinned hourly rate cards,
running/confirmed-stop facts and half-open monthly budgets. It prices only the
interval overlapping the requested month, using integer microunits and charging
open intervals through the read time. The server owns UTC calendar boundaries,
node authentication, persistence and placement refusal; see
[monthly Boat accounting](../../docs/implementation/H2-monthly-boat-budget.md).
