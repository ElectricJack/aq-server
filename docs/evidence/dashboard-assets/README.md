# Optional dashboard asset service checkpoint

Source `14bd108` on `feat/dashboard-static-serving`, based on daemon `d24fc63`.
Root integrated the same source as `26201d9`. The three changed source/test paths
and unchanged workspace/server dependency manifests/lockfile are hashed in
`source.json`. No schema migration, required Config field, worker route,
frontend source or public wire package changed.

The optional production `AQ_DASHBOARD_DIR` is loaded before binding listeners or
starting background owners. Absent means API-only; configured empty/bad bundles
refuse startup. Existing explicit router/listener constructors remain independent
of ambient environment; `build_admin_router_with_dashboard` provides explicit
injection. `Extension<Arc<DashboardAssets>>` is installed across the configured
human router, with `digest()` exposing SHA256 of the exact loaded manifest bytes
for the separate bootstrap projection. API-only mode has no bundle extension.

A trusted operator bundle must contain schema_version1/api_version
`dashboard-read-1`, with unique file keys and exact lowercase SHA256/byte lengths.
The loader bounds the manifest at256KiB, files at512 and total cached content
at32MiB. Only `index.html` and flat `assets/[A-Za-z0-9_.-]+` names with the agreed
js/css/woff/woff2/png/svg/jpg/webp/ico extensions are accepted; maps and HTML assets
are refused. It uses held directory FDs, openat/no-follow/nonblocking regular
single-link files, bounded reads and integrity checks. Public0755 directories and
0644 files are supported; private directory ownership is not required. Operator
installation/manifest contents are trusted, not untrusted user uploads.

Requests never access the filesystem. Asset bodies share immutable Bytes rather
than making per-request copies of the whole asset. Only the exact UI paths from
the root frontend route list receive the index; dynamic IDs are nonempty ASCII
alnum/underscore/hyphen/dot segments, at most128bytes and never dot/dotdot.
Unknown assets, API/machine paths and unknown deep links retain JSON404. No
catch-all filesystem or SPA fallback is installed. Known UI/asset GET and HEAD
require the current Owner/Viewer via existing CurrentUser resolution; hardening,
CSRF, maintenance and admission layers remain shared with human APIs. The machine
router never receives assets. No data/session inspection/control permission is
inferred from permission to load the UI shell.

## Qualification

Final exact-source focused run: **88 passed,1 ignored**, comprising57 server
library tests (including4 new loader tests),13 existing actual JWT/authz tests,
3 new PostgreSQL/router/actual-local-HTTP tests and15 existing worker API tests.
The ignored library case is the existing source-retention export fixture requiring
selected immutable aq-git/Git executables and disposable PostgreSQL.
Strict server Clippy passed with all targets/features and warnings denied;
workspace formatting check passed. Earlier4-loader/3-router passes and the
initial compile/lint logs are also retained; there were no failed checks.

```text
DATABASE_URL=<disposable test-db fixture> CARGO_TARGET_DIR=/home/jkern/dev/agent-q/target CARGO_INCREMENTAL=0 RUST_TEST_THREADS=4 cargo test -p aq-server --lib --test dashboard_assets --test authz --test worker_api --locked --offline
CARGO_TARGET_DIR=/home/jkern/dev/agent-q/target CARGO_INCREMENTAL=0 cargo clippy -p aq-server --all-targets --all-features --locked --offline -- -D warnings
cargo fmt --all -- --check
```

The tests use only disposable random databases in the existing PostgreSQL17
`agent-q-test-db` fixture; its immutable image ID is retained. No other database
or container is mutated. HTTP/JWT key peers are synthetic local listeners. The
new router tests inject deterministic verified identities to exercise actual
current PostgreSQL role/binding checks, while the existing13 authz cases exercise
the real JWT verifier. Tests verify Owner and Viewer access, removed-user refusal,
unauthenticated refusal, HEAD/body/length/headers, exact UI grammar, API JSON,
CSRF/method behavior, unknown/traversal/source-map/unlisted-file refusal and
worker/API-only separation. An actual local TCP listener serves exact retained
index/JS bytes after the source directory is removed.

Loader cases cover exact manifest digest, retained bytes after backing mutation,
corrupt hashes/lengths, unsupported versions, unknown fields, duplicate keys,
symlink/root-directory/hardlink/FIFO refusal, explicit empty configured directory,
manifest/file-count/total-byte quotas and path/ID bounds. Quota tests check the
specific quota refusal, rather than succeeding because a referenced file happens
to be absent. The final run's summed test-process durations were6.56s; Cargo
reported1m21s build time. Neither is a total wall measurement. Strict lint's
1m03s includes waiting for another Cargo process's shared-target lock.

This scoped checkpoint does not claim full combined-workspace or final frontend
compatibility qualification. Root owns the actual frontend bundle, development
proxy, product documentation and combined runtime qualification; the separate
backend projection supplies bootstrap semantics. No deployment/push or live
human/provider action is performed by these checks.
