# Acceptance evidence

This records executable evidence for the Rust component implementation on 2026-10-30. It does not certify production AQ adapters or the design's provisional scale targets. The four pilots are synthetic workflows; their stateful fake host owns domain records independently from runner snapshots.

## Reproduce

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
crates/aq-playbooks-postgres/scripts/test-postgres.sh
cargo run --release -p aq-playbooks-cli --example measure
```

Final validation passed: **116 workspace tests**, **22 PostgreSQL tests** (including one subprocess helper and the crypto unit test also present in the workspace count), and **2 database-backed CLI administration tests**; no failures or ignored tests. Strict workspace Clippy with all features, formatting, diff whitespace checks and authoring-skill validation passed. Test bodies took 5.90 seconds for the workspace, 1.65 seconds for PostgreSQL, and 3.54 seconds for protected CLI administration, excluding compilation and database startup. The disposable container was removed.

The ordinary workspace suite is database-free. PostgreSQL tests require their explicit feature and disposable database; the script enables it, creates PostgreSQL 17, runs actual transaction/process tests, and removes the container. No provider credentials, AQ daemon, worker, Git repository manipulation, or sibling source checkout is needed.

## Evidence map

| Design acceptance | Evidence | Practical limit |
| --- | --- | --- |
| T1 | Compiler reproducibility, canonical golden vectors, source normalization, tampering and malformed sealed-artifact tests | Integral JSON subset; no proof of prose interpretation |
| T2 | Compiler schema/edge/reference/recursion/unsupported-feature tests and outcome-sensitive type narrowing | Parallel foreach rejected; exact or compatible named catalog selection |
| T3 | Generic test command/event registrations and forged/malformed envelope rejection | Production registration authentication belongs to host |
| T4 | Independent runner regressions pin transitive dependencies across activation/restart and compatible catalog changes | Admission freezes compatible selected hashes; no active-run migration |
| T5–T6 | Event identity conflicts, frozen destination deduplication, partial fanout/restart and missing-artifact recovery | Snapshot-backed destination routing |
| T7 | Event-before-wait, stale revision, duplicate, timeout, backlog and restart tests | Host supplies trusted storage acceptance time |
| T8–T9 | Failed CAS before intent commit, lost-response recovery, unsafe ambiguity, stable transport IDs, fresh business IDs, scoped retry counters, absolute command deadlines | External effects are not exactly-once transactions |
| T10–T13 | Three child modes, nested dependency pins, any/all joins, operation joins, cancellation/late results, inherited authority and shared allocations | Named participants support heterogeneous downstream types; unnamed identity-keyed maps remain restricted |
| T14–T15 | Fake-host exact subject/check publication, current destination conflicts, independent hold ownership | No production evidence or Git adapter |
| T16 | Same Engine in simulation; missing fake results block and exploration limits report incomplete coverage | Scripted scenarios, not symbolic proof |
| T17 | Bounded GC, full active artifact closures, unresolved-operation event preservation, catalog history, producer-epoch retirement, replay fences and native tombstone compaction | Explicit host-driven GC and epoch retirement; quota pressure refuses unsafe deletion |
| T18 | Default-deny public projection, actual-backend protection checks, authenticated snapshot/artifact encryption, key rotation, wrong-key/tamper refusal and protected CLI inspection | Host supplies keys; historical plaintext backups/WAL require separate handling |
| T19 | Real PostgreSQL concurrent CAS, immutable bytes, whole-boundary reopen, late transaction rollback, session termination and SIGKILL subprocess tests | Single owner; does not fence external services after a partition |
| T20 | Coding pilot and real Engine+FakeHost tests: creation alone cannot publish, review changes block, accepted assessment/checks permit publication, changed destination requires repair, new observation, candidate and checks, independent follow-up | Explicit synthetic project and retained subjects |
| T21 | Reproducible expression/pilot microbenchmark and PostgreSQL admission benchmark below | Scope/work-class/item fairness regressions pass; production-scale thresholds remain unvalidated |
| T22 | Exact twelve-command catalog; finite five views, nine templates and six change variants; pilot registrations must equal catalog contracts | Synthetic adapters; no daemon auto-registration |
| T23–T24 | Fake-host complete/pre-scan revision checks, trusted evaluation binding, partial positives without clears, suppression/reminders, host commit and message-ack crash/restart | Host restart simulated by serializing its own ledger; runner PostgreSQL is tested separately |
| T25 | Stateful fake-host routing, watcher and reporting pilots; visibility, fallback, cancellation and late completion; shared notification identity refuses changed content | Explicit synthetic contexts/windows |
| T26 | Nested captured iteration, lexical restoration, shared limits, collection identity/order/duplicate checks and frozen finite host pages | No unbounded streams or parallel foreach |
| T27 | Fixed-interval planner stable IDs/cursors, skip/coalesce/bounded catch-up/clock jumps; host occurrence timezone/misfire/configuration revision survives restart | Fake calendar preserves metadata; production timezone calculation belongs to host |

The seventy research policies have a separate [capability disposition](scenario-dispositions.csv). That table is a design mapping, not seventy executed acceptance scenarios.

## Additional implementation regressions

`crates/aq-playbooks-runner/tests/review_regressions.rs` covers independent review findings: compatible transitive pinning, typed joins, independent activation controls, revocation during recovery, bounded candidate examination, active closure retention, expired correlation history, tiny-budget fairness and storage-protection spoof resistance. Runner semantics additionally cover cancellation crash recovery before/after host commit, late outcomes, retained unknown-operation correlations and replay fences after producer retirement.

`crates/aq-playbooks-postgres/tests/protected_indexes.rs` covers authenticated storage, protected inspection, indexed discovery parity, unchanged history preservation, plaintext upgrade with shared artifact references, rotation, native disposal metadata and epoch retirement across reopen. `crates/aq-playbooks-cli/tests/admin_cli.rs` exercises controls, recovery requests, paging, GC, epoch retirement, rotation and keyfile failure through the binary.

The metrics module covers bounded histograms and private-value-free refusal counters; semantic tests exercise duplicate and queue measurements. Operational counters reset per engine process and are exported by the host. Explicit state diagnostics derive current gauges and retained-history latencies without claiming lifetime counts.

## Measurements

Machine: AMD Ryzen 9 5900X, 24 logical CPUs, 31 GiB RAM, Linux 6.18.33.2 WSL2 x86_64, Rust 1.96.1. Results are local observations on a shared development machine.

- Release expression evaluation: 10,000 samples, p95 164 ns for a literal integer equality under bounded evaluator limits. This is a small pure-expression benchmark, not the whole transition path.
- Release coding simulation: 20 samples, 60 recorded transitions, p95 200.263 ms for the complete scenario, including in-memory namespace snapshot cloning/validation. This excludes external host latency and PostgreSQL. A separate `/usr/bin/time -v target/release/examples/measure` run measured p95 183.101 ms and peak resident memory 8,768 KiB.
- PostgreSQL 17.11, Docker loopback, fsync/synchronous commit on: 100 sequential admissions retaining 100 runs, 1,154-byte artifact, final snapshot 134,603 bytes, p50 19.935 ms, p95 26.332 ms, maximum 31.906 ms, 49.74 admissions/second. This run overlapped development builds/tests and exceeds the provisional 25 ms admission budget; it does not establish a release threshold. See [exact workload and reproduction](../crates/aq-playbooks-postgres/README.md).

Indexed scheduling/fairness and bounded retention have dedicated correctness regressions. These small measurements do not establish production-scale performance. Namespace and artifact limits bound admitted work; full snapshot serialization costs grow with retained state. Live AQ acceptance remains blocked by the [missing host services](aq-host-integration.md).
