The adapter owns the `aq_playbooks` PostgreSQL schema and checksum-verified
migration history. `from_client` accepts a host-configured synchronous
`postgres::Client`, including TLS settings; `connect` is the convenience for local,
explicitly unencrypted transport. Database transport encryption and storage
protection are separate configuration choices.

Each namespace has one atomic snapshot and optimistic revision. Receipts, waits,
timers, child ownership, events, retention fences, artifact references, scheduling
indexes, and safe inspection rows commit together. Session advisory locks exclude
other connections, including a connection using the same owner name. Acquisition
records a durable runtime incarnation. A replacement must retire the prior runner;
this ownership model does not fence a partitioned process from external systems.

Native tables index runnable scope heads, due timers, effect scope heads, hashed
correlation postings, waiter IDs, run IDs, and receipt sequences. Scope and effect
item cursors rotate eligible heads; inactive history is not scanned to select one
participant. SQL index/projection maintenance receives only changed rows and
explicit deleted keys, compared with the previous snapshot. Ordering uses the C
collation to match Rust string ordering. Inspection uses keyset cursors, a
1..1000-row limit, and one consistent snapshot revision. Its materialized rows are
finite public projections, including contract-allowlisted result fields. Unknown
receipt details and raw inputs, outputs, variables, and free-form host diagnostics
are excluded. Public metadata inspection intentionally works without decryption
keys; the host must authorize database roles and control-surface access.

The authoritative snapshot is still serialized for CAS. Namespace size is bounded
by runner admission/history quotas; native discovery and incremental index writes
do not make snapshot serialization constant-time. A version-one database upgrades
transactionally; its next exclusive owner initializes materialized indexes before
scheduling or inspection. Reads refuse an uninitialized index rather than reporting
an empty queue.

Protected storage is configured with host-supplied keys:

```rust,ignore
let keyring = ProtectionKeyring::new(active_key_id, host_loaded_keys)?;
let storage = PostgresStorage::connect_protected(&database_url, keyring)?;
```

The key map contains key IDs and 32-byte keys. The keyring has no Debug or serde
representation and zeroizes its owned key buffers when dropped. Keys are never
stored in PostgreSQL. The host owns key generation, access policy, secure loading,
key escrow, backup retention, and deletion. Plain constructors advertise no
protection capability; the runner refuses sensitive root/child/event admission
before recording protected inputs when protection is unavailable.

Snapshots and canonical artifact bytes use
[RustCrypto chacha20poly1305 0.10.1](https://docs.rs/chacha20poly1305/0.10.1/chacha20poly1305/)
XChaCha20Poly1305 authenticated encryption, with a fresh OS-generated 24-byte nonce
per envelope and a 256-bit host key. Authentication binds format, key ID, namespace,
and snapshot revision or artifact digest; snapshot and artifact domains differ.
Protected artifact payloads are per namespace. The plaintext artifact table serves
only explicitly unprotected namespaces; a protected namespace never falls back to
another namespace's plaintext copy. Missing keys, wrong keys, altered ciphertext,
wrong context, and malformed envelopes fail closed. Protected snapshots have a
NULL plaintext column. Public indexes contain only declared safe metadata, not
protected payloads.

To rotate, configure both old and new keys with the new active ID, take exclusive
ownership, and call `reencrypt_namespace(namespace, owner)`. That owner-validated
CAS rewrites the snapshot and all retained artifact envelopes under the active
key. Retain old keys until every backup requiring them is retired. A restored old
backup requires its original keys; a new active key cannot decrypt it. Back up the
entire owned schema consistently. Restoring PostgreSQL does not roll back external
effects; restore operators must reconcile uncertain operations through the host.

Enabling protection on an existing plaintext namespace encrypts its next complete
CAS. Shared plaintext artifact copies are removed only after every referencing
namespace is protected and has its own encrypted envelope. Copies still serving an
unprotected namespace remain intentionally readable. Historical WAL, snapshots,
backups, exported files, and previously exposed plaintext are not retroactively
encrypted; the operator must retire them under its migration and backup policy.
Use protection from namespace creation when source constants are confidential.

Garbage collection requires explicit runner disposal records. It preserves active
and unresolved runs, children/effects, event destinations, callable dependency
closures, and retained audit references. Replay tombstones survive event payload
expiry. Only a recorded increase in the producer's replay epoch permits removal of
that epoch's native event tombstones; stale delivery epochs remain refused. The
injected monotonic GC clock and audit horizon permit compaction of settled
run/operation disposal metadata and unreferenced artifact payload/catalog metadata.
Shared artifact installation and disposal serialize on digest locks. Tombstone
quota pressure refuses the transaction without deleting work. Artifact addresses
remain content verified, including a byte digest while their disposal tombstone is
retained.

Run the required storage and protected CLI suites with an isolated Docker database:

```sh
crates/aq-playbooks-postgres/scripts/test-postgres.sh
```

The script creates a unique disposable PostgreSQL container, runs both suites, and
removes it. Alternatively set `AQ_PLAYBOOKS_TEST_DATABASE_URL` to a disposable
PostgreSQL database and run `cargo test -p aq-playbooks-postgres --features
postgres-tests`, plus `cargo test -p aq-playbooks-cli --features postgres-tests
--test admin_cli`. The suite includes fault injection, backend termination, an
abrupt subprocess kill, migration from schema version one, scheduling/inspection
races, shared-artifact GC races, encryption tampering, sensitive Engine dispatch,
key rotation, backup restoration, and repeated replay-epoch reclamation. Never use
a production database for these tests. Core tests require no database.

The historical 2026-10-30 snapshot-only admission workload measured 100 sequential
roots at p50 11.960 ms, p95 15.281 ms, maximum 20.096 ms, and 83.50 admissions/s;
its 1,154-byte artifact and final 84,782-byte state preceded native indexes and
protection. Current measurements are recorded below after running the same
`cargo run --release -p aq-playbooks-postgres --example admission_benchmark`
workload against an empty disposable database. Setup, compilation, migrations,
installation, and activation are excluded; no host effects execute. This workload
is evidence for its declared size, not a scale claim or release threshold.

Current native-index/retention workload, 2026-10-30: PostgreSQL 17.11
`postgres:17-alpine` Docker, loopback TCP, `fsync=on`, `synchronous_commit=on`;
AMD Ryzen 9 5900X (24 logical CPUs), 31 GiB RAM, Linux 6.18.33.2 WSL2 x86_64,
Rust 1.96.1 release build. The 100 root admissions retained one 1,154-byte artifact
and ended with a 134,603-byte serialized state. This unprotected workload ran
alongside team compilation/tests; it is not an idle-machine release measurement.

| Measurement | Result |
| --- | ---: |
| Median admission | 19.935 ms |
| p95 admission (nearest rank, 95/100) | 26.332 ms |
| Maximum admission | 31.906 ms |
| Total measured admission time | 2,010.265 ms |
| Throughput | 49.74 admissions/s |

The observed p95 exceeds the provisional 25 ms budget by 1.332 ms; this report
makes no claim that a production latency threshold has been met. Peak memory and
protected-write throughput were not measured by this workload. The current
required PostgreSQL suite passed 22 tests (one subprocess helper), and the
protected CLI suite passed two tests, with zero ignored. A final combined run
reported 1.65 seconds for PostgreSQL tests and 3.54 seconds for CLI administration,
excluding compilation and disposable database startup.
