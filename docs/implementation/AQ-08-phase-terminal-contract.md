# Phase terminal protocol candidate

Source checkpoint `baabe2e3fbc1e10602cba1fddf7185e24bc483c4` on base `92f44e3` adds
only `aq-phase-terminal-protocol`0.1.0 and its workspace member/lock entry. This is
the pure contract slice of the [consumer plan](AQ-08-phase-terminal-consumers-plan.md).
It activates no route, database migration, worker producer or physical capability.

The [crate README](../../crates/aq-phase-terminal-protocol/README.md) defines the
portable API and owner boundary. Started, matching Stopped and both no-Started
closure forms bind the actual phase execution/accepted receipt identities while
reusing unchanged frozen body types. The borrowed context validates the complete
phase chain and exact retained server Prepared checkpoint. No legacy
`GrantedContext` is constructed. Whole-envelope domains and acknowledgment
request digests prevent origin/negotiation reassociation. Every wire document is
bounded to64KiB inclusive, with stricter frozen inner limits preserved.

A lost-first-Grant worker can query by its actual accepted receipt digest, Prepare
operation and exact binding. The finite response carries original pins, versions,
Prepared checkpoint, hard deadline and projection sample only. It omits the
potentially128KiB execution receipt and every delivery clock/live object. Null
identity means snapshot absence only and may later become known for the same
lookup. It never proves NeverGranted. Authenticated current lookup, physical
proof, no-Started state, newer acknowledged journal counters and source/effect
settlement remain separate owner checks.

Qualification:12 pure tests pass0.22s; extracted exact-archive12 pass0.24s. Strict
all-target/all-feature Clippy and formatting pass in both workspaces. External
JSON Schema validates16 valid documents and rejects4 invalid shapes;4 additional
relational negatives are refused by Rust. Tests cover both closure evidence
forms, current-history mismatches, original logical boot/generation/runtime,
false ECHILD/guardian/pane, immutable outer request identity, exact negotiation,
strict extra/duplicate/float rejection, inclusive byte boundary, lost-reply known
keys, required nullable absence and historical replay without live clocks.

The retained [archive](AQ-08-phase-terminal-contract-evidence/aq-phase-terminal-protocol-0.1.0.crate)
is43,951bytes, SHA256
`1b3e4afbd9356f6d7027b0a4be3fe37e2231b55bdfa10269c87a84bf9568f0bb`.
It was packaged with `cargo package -p aq-phase-terminal-protocol
--exclude-lockfile --no-verify --offline` from clean source. VCS metadata names the
exact source commit. The only package warning is absent optional registry metadata
(including an unassigned license); no legal terms were invented.

The standalone consumer extracted that archive and the three existing frozen
[admission019 dependency archives](AQ-08-admission019-evidence/archive-manifest.json).
Its root manifest alone patches crates.io paths; no extracted manifest/source was
edited. An additional consumer with `serde_json/preserve_order` confirms the exact
contract artifact and complete request digest stay unchanged. All16 original crate files compare equal (the normalized manifest retains
its unchanged original alongside it), and all14 admission source modules compare
byte-for-byte with frozen019. The old017/018/019 archives and module bytes remain
untouched. This qualification performs no network or external service calls.

[Evidence](AQ-08-phase-terminal-contract-evidence/report.json) retains package and
source hashes, consumer Cargo manifest/lock, source/extracted test/lint/fmt/schema
logs and a superseded test-only borrow-check compile failure corrected before the
final source freeze. Test timings are test-process timings, not an operating
capacity claim. Normal fixture observations are synthetic correlations, not actual
host compatibility or daemon authority qualification.
