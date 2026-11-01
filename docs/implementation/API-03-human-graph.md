# API-03 first human Graph read increment

The human Owner can inspect a configured project's actual task and knowledge records through AQ. Source is `agent_access::human`; root integration mounts its routes inside the existing human router and publishes the paired capabilities. This increment adds no database tables, credentials, external service or shadow task store.

The [public human-graph-1 contract](../../protocol/human-graph/v1/README.md) and schema describe bounded lists/search, task and knowledge detail, one-hop task dependencies and exact knowledge content revision reads. Current project policy remains Owner-only; session inspection permission does not expand it. Private arbitrary node metadata and Graph service credentials are excluded from the projection. User-authored body/provenance remains authorized project content.

Validation used a separately launched, explicitly pinned aq-graph ELF and disposable real PostgreSQL databases. Public Graph mutation requests created the task pair, blocking edge and knowledge record. The human routes then verified list/pagination, exact detail and revisions, dependency edges, search, contract shape, service-token/metadata exclusion, cursor kind/project revision refusal and current Owner revocation while an actual Graph response was held. Two cases passed in 1.77 seconds. The cursor unit separately covers principal, project, filter/limit, original expiry, tampering and router-restart fencing.

Explicit Graph binary SHA256: `eb9c8f41002cb5d9675f87de0cadbe34e7d74cfc60d2449cd23ac4df76921fea`. The test accepts a caller-selected binary path only with its independent expected digest. This does not claim that the binary was rebuilt from the current sibling source tree.

Commands (reuse the assigned server target exclusively; initial compile separate):

```sh
CARGO_TARGET_DIR=/tmp/aq-native-union-target CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo test -p aq-server --test human_graph --no-run
DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres AQ_TEST_GRAPH_BINARY=/absolute/pinned/aq-graph AQ_TEST_GRAPH_SHA256=<verified-digest> /absolute/copied/human_graph-test --include-ignored --test-threads=1
CARGO_TARGET_DIR=/tmp/aq-native-union-target CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo clippy -p aq-server --lib --test human_graph --all-features -- -D warnings
CARGO_TARGET_DIR=/tmp/aq-native-union-target CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo test -p aq-server --lib agent_access::human::cursor::tests
```

Earlier fixture invocations correctly refused the unsupported `create_edge` operation and unnamespaced metadata. The actual revision test exposed Graph's `RevisionSnapshot` wrapper; the adapter now validates that retained contract before projecting its node. No deadline or upstream validation was relaxed.

Root and the UI owner must qualify the compiled bundle against the mounted routes before claiming live Work/Knowledge delivery. The module alone does not enable capabilities. History listings, typed writes, full cross-project aggregation, current readiness/effect/settlement and large-history/fleet performance are separate increments.
