# Current Playbooks catalog consumer source qualification

Backend source is `6129bed` → `b5f226b` → `1be7cf0` → `056551f` on isolated
base `ba65fbe` (state schema36); UI source is `555b76e` → `fd6e7ab` → `681691f` → `9adb81a`.
The exact new component archive is vendored separately, with runner/Postgres/CLI
0.1.3 and host0.1.3. All old source archives/closures remain unchanged. See
[the route/design contract](../../API-05-current-catalog.md) and
`qualification.json` for exact source, schema, fixture and check anchors.

Final checks passed: three owner-interface/lifecycle cases, three pure API
contract cases, three actual protected PostgreSQL/owner/HTTP cases, scoped server
and host strict lint, schema regeneration and owned-source formatting. Sixty-one
finite-client/adapter/component/unchanged-fallback checks and full TypeScript
checking passed. Earlier existing-host, actual protected-host restart and loaded/
inert tests are recorded separately with their source-stage limits.

Independent review caught stopped capability capture, UUID-only principal
checking, an unbounded browser body and an exact409 stale-roster edge. The fixes
use a weak live support handle, original trusted User/row-version checks before
and after the owner wait, true403 authorization denial, a finite headers-first
stream client, a refused-context fence, and a pre-paint capability-loss epoch/cache fence. Actual held subject/issuer/role/
row-version vectors include an engine-error wait; no metadata is returned after
denial. A separate negative exact-cursor case and a precisely full component
budget exercise AQ's shorter-page cursor packing.

`server-strict.log`, `host-strict.log`, `unit-final.log` and `schema-final.log` are
verbatim final outputs. Other tool results in JSON are curated, not invented raw
logs. Whole-workspace rustfmt finds the existing unmodified base's
`profiled_results.rs` module order; the owned Rust paths pass and that unrelated
root-owned test file was preserved.

Independent scoped source review accepted backend056551f and UI681691f/9adb81a
with no remaining concrete blocker; execution results are the author
qualification recorded here, not reviewer-run tests.

The pinned disposable test binary can serve the root-assembled compiled bundle
through the actual protected owner. The browser probe verifies two real retained
artifacts, mail binding revision2, 128-byte definition/entry/dependency names,
exact pins, controls, deep-link reload, desktop/390/320px exits/focus and no
mutations, unrelated engine reads, historical fallback or foreign requests.
That compiled-browser and independent visual acceptance is still pending. This
source fixture's forwarding transport is explicitly synthetic; it does not
replace the independently managed actual UDS deployment qualification.

No canonical database/configuration, private UDS cluster, runtime namespace,
remote publication, provider, worker or external-message effect changed. This is
not schema37 merged-stack proof, canonical promotion, full Playbooks editing or
fleet performance closure.
