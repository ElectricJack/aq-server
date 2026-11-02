# Opt-in historical phase result metadata

Base: `cd64196`. This bounded API slice reads existing qualified B1 records;
it creates no report, proof, acceptance, completion, media grant or new SQL owner.
The implementation remains isolated pending focused qualification and actual
execution integration review.

The existing `GET /api/human/results` list and exact revision route currently
filter/refuse phase origins even though `results::load_locked` verifies actual
Legacy, Frozen and Refresh historical owners. Its public coverage literal is
`recorded_legacy_native_only`. The current dashboard and supplemental media owner
deliberately enforce that boundary. Broadening those pages under the old literal
would be false; changing the default literal would break the existing client.

## Additive profile

Root approved an opt-in query `profile=recorded_native_v2` on the existing list
and exact revision routes. An absent profile preserves current v1 behavior,
coverage, field set and phase refusal. Unknown profiles refuse rather than
silently falling back. The v2 page uses honest coverage `recorded_native_v2` and
the same page/metadata shape, limits, UUID keyset ordering, exact report digest,
Owner authorization, no-store response and bounded work budget.

Root approved this minimal additive v2-only metadata field:

```json
{"origin":"legacy_prepare_v1 | phase_execution_v2 | phase_refresh_execution_v1"}
```

`origin` is one closed enum string per item, derived exclusively from the generic
historical `RecordedResult.origin` after full owner verification. The field is
omitted in default v1 responses. No new origin comes from caller input, launch
labels, current allowlists or a raw result-row string. The existing
`schema_version:1` record structures otherwise remain unchanged; the explicit
read profile/coverage selects the additive interpretation.

Without per-item origin, a mixed v2 list cannot safely drive downstream
origin-specific capabilities. The field enables future adapters to distinguish
those domains while keeping all phase media actions unavailable. The legacy-only
media owner and frozen media contracts are unchanged. A future v2 client must
obtain an actual legacy v1 binding before using existing legacy media APIs; a v2
coverage string is never rewritten as legacy coverage. No UI edits or capability
advertisement are part of this slice.

## Owner and source associations

Discovery selects bounded report IDs joined to their actual launch project.
Default v1 keeps its phase-attempt exclusion. Opt-in v2 discovers all reports;
every returned item goes through `results::load_locked` in the existing bounded
Owner transaction. That loader verifies report/receipt bytes, nominal issued
execution and Started/Stopped history, registration/qualification, exact spec and
launch binding, outcome artifact linkage and report head. A malformed origin or
foreign association fails the read, never becomes an empty successful page.

The public repository/base and source/effect IDs remain projections of the
verified exact spec/report. Supplied receipt IDs are references, not proof of
retention, source coverage or settlement. This slice does not broaden the source
retention owner, synthesize checks, or infer B2/C outcomes. Historical phase
records remain readable after execution expiry/runtime retirement; stale exact
report digests and foreign project scopes still refuse.

## Focused qualification

Reuse the actual disposable Frozen and Refresh issuance/terminal/result owners
from `phase_results` with explicitly synthetic proof DTOs. Add one focused router
case per family covering opt-in list/exact detail, honest origin/coverage,
repository/base/source references, corrected head and UUID pagination, default
v1 omission/refusal, foreign project/revision, unknown query/profile, and current
Owner revocation. Reuse existing corruption vectors through the new public read
to prove nominal owner verification is retained. Add opt-in v2 assertions to the
two existing legacy human result tests. Run only these focused cases plus the
affected strict checks under an assigned lease, not the full phase pipeline.

No canonical schema, service, database, dashboard bundle or project is changed.
Actual provider/physical execution and phase media remain separately qualified.
