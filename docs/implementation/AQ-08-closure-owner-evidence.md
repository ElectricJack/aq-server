# Granted closure owner qualification

Source: `7eb9696`, parent `5fd85be` (renewal `c5438d3` equivalent), with
material/budget `9f653f3` (`f5814b5`) and initial-Grant fence `087ee84`
(`aabbf18`). Migration 028 remains byte-identical to prerequisite `75015d0`;
its earlier smoke/source evidence is `98453a9`. No main merge or production
activation was performed by this lane.

The exact final source passed nine owner SQL/HTTP cases and three schema cases,
zero failures/ignores: 10.52s owner execution and 1.81s schema execution, after
33.54s build. Strict server all-target/all-feature Clippy passed in 45.66s;
workspace fmt check passed. No total wall time was measured. The test command
was `cargo test -p aq-server --test granted_closure --test schema28_closure
--locked --offline`; lint was `cargo clippy -p aq-server --all-targets
--all-features --locked --offline -- -D warnings`.

The dedicated target was `/tmp/aq-closure-owner-target`, with
`RUST_TEST_THREADS=2`, `CARGO_BUILD_JOBS=2`, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, and `CARGO_PROFILE_TEST_DEBUG=0`. Disabling debug
symbols leaves assertions and production limits unchanged. Tests used new
per-test databases on the pinned disposable PostgreSQL container at port 55432;
exact image/toolchain/source hashes are retained below. This is correctness
qualification under shared host load, not throughput or startup-capacity proof.

The owner cases exercise actual publication → Accept → initial Grant, both closure
variants, exact replay/changed operation conflicts, current policy replacement
and retirement, injected insert failure rollback, expiry, credential rotation
during an observed node-lock wait, actual Started/closure competition, human
listener separation, immutable new SQL history and reserved admission capacity.
They preserve held task claims, original grant and Unknown physical phase.
The schema cases cover fresh empty/default-disabled startup, actual 27 → 28
transaction rollback/refusal and a global quota race: two different nodes wait
on the advisory lock at 4095 rows, then exactly one insert succeeds at 4096.
That fixture uses four connections for barrier, two writers and observer.

These are actual PostgreSQL/HTTP owners with synthetic runtime/proof policy.
They do not qualify a physical worker suppression or host-proof producer,
activate the default route, perform host launch/renewal, release task/source or
effects, or change frozen public 0.1.7/0.1.8 bytes. The optional route requires
explicit trusted policy installation/injection; runtime startup activation and
worker adoption remain separate. See the [owner scope](AQ-08-closure-owner-candidate.md)
for finite history caps, exact byte metric and existing append-only application
trust boundary for old prepare/renew receipts.

Retained negative development results are not passing evidence:

- Initial owner run: 3/9 passed; valid closures 500. Incoming PostgreSQL
  `octet_length` sum was INTEGER but decoded as i64; explicit BIGINT cast fixes
  it. A pinned read-only type probe is retained. The expiry fixture also queried
  the deadline as a column rather than inside hot JSON and was corrected.
- A temporary diagnostic run printed only SQLSTATE/constraint and fixed guard
  strings; no messages appeared because the error was a result decode mismatch.
  Those diagnostics were removed before final qualification.
- A subsequent shared-target build could not find source-present modules/router
  methods. Root independently saw mounted routes 404 from the same shared target.
  These inconsistent-artifact symptoms were isolated by a fresh target; their
  precise Cargo/cache mechanism is not claimed proved. The failed compile is
  retained as harness evidence, not a product-source result.
- First isolated run: 7/9 passed; fixture expectations incorrectly required human
  fallback 404 without full CSRF headers and overload 429 rather than existing 503.
  Next run 8/9 still lacked `X-AQ-Request: 1`; both headers were then supplied.
  Production auth/admission behavior was preserved.
- The first quota-race fixture used the generic two-connection test pool and
  could not admit both writers plus observer while holding its barrier. Only
  that fixture now explicitly allocates four connections; no deadline/quota or
  production pool was widened. The final race passed.
- Initial strict lint found one collapsible condition. A mechanical let-chain
  correction was followed by strict lint and the complete exact-source 12-case
  rerun retained as `owner-frozen-tests.log`.

Raw logs, qualification metadata and source inventory are retained at
[evidence/closure-owner-7eb9696](evidence/closure-owner-7eb9696).

For reruns, use the leased slot target and current dev/test profile from
[Cargo worker build guidance](../guides/cargo-worker-builds.md), with
`CARGO_INCREMENTAL=0`. The paths and debug overrides above record historical
runs; the original evidence is retained.
