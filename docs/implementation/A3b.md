# A3b: aq-git v0.2.0 consumer pin

Status: release pin, image builds and named consumer qualifications pass. The
server and worker images contain the exact released aq-git executable. Workspace
acceptance did not pass: the full no-fail-fast run completed with 1,393 passed,
32 failed and 30 existing ignored tests across 225 target/doc-test results.
Clean-main comparison and unchanged serial rechecks resolve all 32 failures:
19 reproduce identically on main, 12 pass unchanged in isolation/rechecks,
and one passes with its required Graph fixture configured. Supervisor message
`msg-ca59a3e2ee0142fda8c632fbd677fb97` explicitly accepts that disposition.
No passing full-workspace result is claimed.
A3b depends on PR #19 landing first.

## Inputs and implementation

The published [v0.2.0 release](https://github.com/ElectricJack/aq-git/releases/tag/v0.2.0)
points at `e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a`. GitHub's release asset
metadata independently confirms the retained bytes:

| Input | SHA-256 |
| --- | --- |
| Release source archive | `f64891f03e2d28263cf3af20b38123935ae08d6917699becb9d77e76af2fd3c6` |
| Linux x86_64 executable | `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2` |
| A3a build record | `94e318a2285369002c1df8ea9844ca0295ce3b867fa35691d201cc77f1c7852f` |
| Retained Git bundle | `217d925d105b12a1fcd40b02a4a0a24b0c3cfc31a98cdc46d72918a94debd084` |
| Unchanged upstream Git 2.56.0 archive | `26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3` |

The first three files are byte-preserved copies of A3a's downloaded assets.
The bundle was created with `git bundle create <vendor/aq-git-source.bundle> HEAD`
in A3a's clean detached release checkout. Its HEAD is the published commit;
verification clones the bundle into temporary bare storage and compares every
committed source blob with the archive inventory. The consumer never reads a
sibling checkout or fetches a floating source revision.

`vendor/git-candidate.json` records release, executable, build-record and inventory
paths plus digests. `vendor/git-candidate.sha256` covers archive, bundle,
executable, build record and upstream Git. The new versioned vendor directory
preserves upstream Git and its checksum provenance, and inventories all 366
release source files. The checker validates archive safety before reading
members, embedded commit, locked package version, build record provenance,
source inventory and retained bundle. Official overrides remain refused.

Both application Dockerfiles embed the canonical aq-git stages byte-for-byte.
They verify the release archive and executable with `sha256sum -c`, then copy
the executable's exact bytes. Copying the released executable meets the consumer
release pin without requiring a reproducible Rust rebuild. Both stages preserve
`rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663`.
A3b does not change non-aq-git base stages, state labels or Compose supervision.
The later main merge retains concurrently landed changes outside this scope.

The project-binding wrapper selects the newly built immutable image ID and runs
its named Rust qualification with `--locked`. The existing fixture-derived
checkout selection is preserved. `docs/source-retention.md` records the new
commit, binary digest and release-copy choice.

## Environment and commands

See [environment.json](A3b-checks/environment.json). Commands were run in the
assigned task worktree with Rust/Cargo 1.96.1, Python 3.12, Docker BuildKit and
disposable PostgreSQL 17. Rust commands use `CARGO_BUILD_JOBS=2` and
`RUST_TEST_THREADS=1`, except the supervisor-directed workspace retry uses two
test threads; Python behavior tests use AQ's unchanged worker defaults.
`DATABASE_URL` for database tests selects only the disposable database started by
`scripts/test-db`.

| Command | Observed result |
| --- | --- |
| `aq test scripts/test-git-candidate.py` before implementation | 5 failed, 3 passed: missing executable/provenance/path validation and old release pin; [red log](A3b-checks/verifier-red.log) |
| `aq test scripts/test-git-candidate.py` after implementation | 10 passed, including release acceptance, executable tampering, provenance tampering, unsafe paths, archive traversal, inventory mismatch, bundle/source disagreement, official overrides and Docker drift; [post-repair green log](A3b-checks/verifier-after-repair.log) |
| `python3 scripts/check-git-candidate.py --official` | Passed; [final verified manifest](A3b-checks/candidate-final.json) |
| `gh api repos/ElectricJack/aq-git/git/ref/tags/v0.2.0` | Published tag resolves to retained commit; [tag receipt](A3b-checks/release-tag.json) |
| `gh api repos/ElectricJack/aq-git/releases/tags/v0.2.0` with asset name/digest projection | Published source/binary/build-record digests match; [asset receipt](A3b-checks/published-assets.json) |
| `DOCKER_BUILDKIT=1 docker build --progress=plain -f scripts/Dockerfile.aq-git -t aq-git-a3b:0.2.0 .` | Passed; [build log](A3b-checks/build-aq-git.log) |
| `DOCKER_BUILDKIT=1 docker build --progress=plain -t aq-server-a3b:0.2.0 .` | Passed after prerequisite repair; [server build log](A3b-checks/build-server-after-repair.log) |
| `docker build --progress=plain -f Dockerfile.worker --build-arg AQ_SOURCE_REVISION=3fb2bcb2f63f73838ddd97e44580fe721a539bdd --build-arg AQ_WORKER_ARCHIVE_SHA256=ed33076d62d37884cab04f1206d1fda3dd5e06181ddb1ccdebec58fda789b535 --build-arg AQ_VERSION=0.3.1 -t aq-worker-a3b:0.2.0 .` | Passed; [worker build log](A3b-checks/build-worker.log) |
| `docker build --target aq-git-build` with a deliberately corrupted retained executable in a disposable context | Correctly refused by `sha256sum -c`; [negative build log](A3b-checks/corrupt-binary-build.log) |
| `cargo fmt --check` | Passed; [final format log](A3b-checks/fmt-final.log) |
| `cargo clippy --locked --all-targets -- -D warnings` | Initial post-repair run exposed a duplicate attribute in phase_never_granted.rs:4/6; eager-grove-45.3.2 removed it. Passed on final prerequisite stack; [final Clippy log](A3b-checks/clippy-after-reservation-repair.log) |
| `cargo test --locked --workspace` | Not passed: two serial attempts failed on different grant/permit expiry cases; [first repaired-stack run](A3b-checks/workspace-after-reservation-repair.log), [serial retry](A3b-checks/workspace-final-retry.log). All 12 repaired reservation tests pass |
| `cargo test --locked --workspace -- --test-threads=2` | Exit101: a third closure expiry; [retry log](A3b-checks/workspace-test-threads2.log). Later suites were not reached by these fail-fast runs |
| `cargo test --locked --workspace --no-fail-fast -- --test-threads=2` | Exit101: 1,393 passed, 32 failed, 30 existing ignored across 225 target/doc-test results and 14 failing targets; [complete log](A3b-checks/workspace-no-fail-fast-final.log), [every target's result](A3b-checks/workspace-suite-results.md) |
| `cargo test --locked -p aq-server --test granted_closure -- --test-threads=1 --nocapture` | Passed unchanged: 10 passed, 0 failed, 0 ignored; [serial suite log](A3b-checks/granted-closure-serial.log) |
| `cargo test --locked -p aq-server --test source_retention --test project_bindings` | Passed: project_bindings 12 passed/1 ignored; source_retention 10 passed/1 ignored. Ignored cases run separately; [focused log](A3b-checks/focused-rust-after-repair.log) |
| `AQ_TEST_GIT_CANDIDATE=/tmp/aq-a3b-qualification/aq-git AQ_TEST_GIT_EXECUTABLE=/tmp/aq-a3b-qualification/aq-git-runtime/bin/git cargo test --locked -p aq-server --test source_retention selected_real_aq_git_import_survives_worker_volume_loss_and_base_download_is_scoped -- --ignored --nocapture` | Passed: the named ignored test ran, 1 passed; [qualification log](A3b-checks/source-retention-after-repair.log) |
| `python3 scripts/test-project-bindings.py` | Passed: selected_real_public_repository_and_graph_workspace_bind ran, 1 passed; pinned graph guards also passed and synthetic resources cleaned up; [wrapper log](A3b-checks/project-bindings-after-repair.log) |

## Embedded executable checks

For each successful image, `docker run --rm --entrypoint sha256sum IMAGE
/usr/local/bin/aq-git` equals the release binary digest. Separate invocations
of `/usr/local/bin/aq-git --version` and `/opt/aq-git-runtime/bin/git --version`
report `aq-git 0.2.0` and `git version 2.56.0`.

| Image | Immutable image ID | Embedded aq-git SHA-256 |
| --- | --- | --- |
| Standalone `aq-git-a3b:0.2.0` | `sha256:4e405e60f637f766582fcb182d1e3bd963ba8e78c94d2cc57203d28f0f358642` | `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2` |
| Worker `aq-worker-a3b:0.2.0` | `sha256:bfb1309ad2f7fc1765ff0526b0b568882b4611f05981c5c9841120375d08fbf1` | `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2` |
| Server `aq-server-a3b:0.2.0` | `sha256:2dc0cb33fdc8e960a08204d6dadf34162b92eade587c268d0ee9d4c3a46163f8` | `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2` |

Machine-readable results: [image-digests.json](A3b-checks/image-digests.json).

## Prerequisite integration and limits

Initial attempts at source base `ab314605` failed with duplicate DeliveryClock
methods in aq-admission (E0592/E0034). Required child `eager-grove-45.3.1`
reconciled those implementations in `d6ed24319745d8030e71c526b498b83f5bdde5e0`.
The authorized `aq git pull --branch main --json` integrated that repair without
conflicts in merge `912155a13392ab4a3176d2202acf9355b5170d1c`. Post-repair Rust commands and the server build use that merged source. The initial failure
logs remain in `A3b-checks/`; the `*-after-repair` logs record current results.

Strict Clippy then exposed a pre-existing duplicate `allow(dead_code)` attribute
on the same terminal fixture module in `phase_never_granted.rs:4/6`. Follow-up
`eager-grove-45.3.2` removed the redundant line without changing assertions or
fixture behavior. Fetched main `c13cf4bf` was merged without conflicts as
`1cf77fc1`; that integration also retains other concurrently landed main changes.
The pinned aq-git build blocks and retained release inputs remain unchanged.

Workspace tests then failed in nine global mail-reservation unit tests, all at
`src/agent_access/messaging/global/reservations/tests.rs:99`. Their fixture
explicitly uses registered schema37, but its seed calls `writes::enqueue`,
which now delegates to the dashboard action journal. The journal queries
`dashboard_operations`, introduced by migration0042. The returned error is
`IdentityUnavailable("dashboard operation unavailable or conflicting")`.
Required child `eager-grove-45.3.3` reconciles that historical fixture with action
routing. The repair is on `aq/eager-grove-45.3.3` in unmerged PR #19; a human owns
that PR's merge. Supervisor message `msg-41efaa94f6ce40f093a85cb3b25549b1`
explicitly authorizes stacking A3b on the prerequisite branch. Guarded
`aq git pull --branch aq/eager-grove-45.3.3 --json` merged it cleanly as
`46fb7a19db91d3f6cd08fa2c46084c7cf507c0f7`. A3b depends on PR #19 landing
first. The repair adds only a `cfg(test)` adapter through the existing mailbox
writer, preserves registered schema37 and asserts the dashboard journal remains
absent. The child's focused historical and current-route regression evidence is
retained in [schema37-reservation-checks](schema37-reservation-checks/README.md).
Strict Clippy passes on the repaired stack. The first workspace run on that
stack passed all 12 reservation cases, then failed in
`granted_closure::actual_grant_closes_once_and_replays_without_release_or_execution_clock`:
`granted_closure.rs:18` unwrapped `Conflict("native initial grant is not currently available")`.
The suite reported 9 passed and 1 failed; the complete
[failed run](A3b-checks/workspace-after-reservation-repair.log) is retained.
An unchanged isolated reproduction passed (1 passed, 9 filtered, 2.39 seconds);
see [reproduction](A3b-checks/granted-closure-reproduction.log). The fixture uses
wall time in its clock, making timing under load a possible explanation, not a
proven diagnosis. No source or test edits were made for this failure.

The next serial workspace run passed that exact test, then failed the different
`renewal_counter_fences_old_suppression_and_closure_fences_all_later_authority`
case at `granted_closure.rs:222` with
`Conflict("continuing permit expired, changed, or cannot be extended")` (9 passed,
1 failed). Its [log](A3b-checks/workspace-final-retry.log) is retained separately.
Supervisor message `msg-c2c0462778e7417fa3401f21ddb350d0` identifies the existing
shared-PostgreSQL timing investigation `smart-delta-18` and requests an isolated
serial suite followed by the workspace with two test threads. The full isolated
`granted_closure` suite passed unchanged: 10 passed, 0 failed, 0 ignored in
28.02 seconds, including both prior failures. The requested
`cargo test --locked --workspace -- --test-threads=2` then failed a third case,
`started_and_closure_serialize_to_exactly_one_terminal_history`, at
`granted_closure.rs:18:43` with
`Conflict("selection expired during admission locks")` (9 passed, 1 failed).
Both prior failures passed in that run. UTC timestamps, thread counts and exit
codes are retained in [timing-triage.json](A3b-checks/timing-triage.json).

Supervisor message `msg-e40950af47e1431dbacdefa146c507cd` identifies these as
pre-existing nondeterministic wall-clock expiries, adds the evidence to
`smart-delta-18`, and accepts closure with the passing serial suite
and honest workspace exception. Follow-up message
`msg-657001f6e1834a9481ff02c620d6ff92` clarifies the condition that all other
suites must pass and requests a no-fail-fast workspace run. That run completed
with all 225 target/doc-test results retained in the
[full table](A3b-checks/workspace-suite-results.md): 1,393 passed, 32 failed,
30 existing ignored tests. `granted_closure` passed all 10 in that run.
No tests were edited or newly skipped. The separately executed A3b
source-retention and project-binding suites and both real-component
qualifications pass. No passing full-workspace result is claimed.

Failures beyond timing include historical-schema setup HTTP 500s, protected
settlement release refusal, historical table-drop dependency assertions and a
G6 test requiring explicit Graph fixture variables. Supervisor messages
`msg-4cf0da20c35a40b4a019eea12af0429f` and
`msg-ce51c803e4864b1593f360e5bf915094` require exact serial isolation and a clean
fetched-main comparison for persistent or ambiguous failures. All 32 exact
serial A3b reruns finished: 9 passed, 23 failed; see
[branch receipts](A3b-checks/isolations-branch.json). The Graph-dependent test
passes with an actual binary extracted from the existing immutable qualification
image; [binary provenance](A3b-checks/graph-test-binary.json) records image,
source and binary digest. Both branch and main reruns use that same pinned
binary, synthetic database, two Cargo jobs and one test thread.

Clean main at `ea28da1ca62184e9f5f945b73b62320edbe903e0` was freshly fetched and
checked out detached in task-owned temporary storage. The same 32 exact tests
finished there: 13 passed, 19 failed. [Main receipts](A3b-checks/isolations-main.json)
record source, commands, UTC times and outcomes. All 19 persistent failures have
identical panic locations and messages on branch and main; see the
[exact comparison](A3b-checks/persistent-failure-comparison.json). These include
the preparation-expiry assertion, historical setup HTTP 500s, settlement guard
and FK-drop failures. The clean temporary checkout was verified clean and removed
after its test processes finished; the assigned branch was never switched.

Of four first-isolation differences, three passed on the original unmodified
A3b test executable when repeated serially. The remaining concurrent-grant case
passed both further bounded paired rounds on the original A3b executable and
the main executable (four actual one-test passes). Receipts record executable
SHA-256, commands and UTC times; the branch binaries predate the baseline build.
See [difference rechecks](A3b-checks/branch-difference-rechecks.json) and
[paired rounds](A3b-checks/concurrent-grant-paired-repeats.json). No fixture clock,
test assertion, timeout, execution setting or production code was weakened.

The [32-case disposition table](A3b-checks/baseline-comparison.md) reconciles
every failure: 19 identical pre-existing failures, 12 cases passing
unchanged isolation/rechecks on both, and one resolved missing Graph setting.
No persistent A3b-only failure remains. Supervisor ruling
`msg-ca59a3e2ee0142fda8c632fbd677fb97` explicitly allows closing with identical
clean-main failures recorded as pre-existing and timing failures accepted under
`smart-delta-18`. The complete paired result was reported before closure. This
is an accepted workspace exception, not a passing full-workspace qualification.

A diagnostic `cargo test --locked --workspace --no-fail-fast` run was started
to expose failures beyond the original early stop. It was interrupted with
exit130 during compilation when the repair task reported completion, before
modifying the compiling checkout. This diagnostic is not a passing test result;
its [log](A3b-checks/workspace-all-results.log) is retained. No compiler remained
active when the main or prerequisite merges ran.

Supervisor message `msg-c6f2114e7cd64deeb8ab1aed23c51a9c` explicitly waives
the completion profile's missing `origin/dev` ref: fetch and merge `origin/main`,
never create `dev` or push `main`. That synchronization was performed through
AQ's guarded git command.

A3b is not marked for adversarial review. The executable is Linux x86_64 and its
recorded maximum GLIBC requirement is 2.34. No new hosted deployment, performance,
host-power-loss or exhaustive compatibility qualification is claimed. The retained
A3a build record intentionally preserves its original preparation-time status.

The implementation plan was committed before code as `32ff7432`; the
unreviewed feature history was then consolidated before final integration.
The consolidated implementation commit is `0229a53f771cec234198ac38ecdc6ed846692aa2`.
The prerequisite merge preserves its ancestry. Final evidence is committed after
the merge; its exact SHA is recorded in the AQ task comment and guarded push
receipt. No default-branch delivery is claimed by a worker push.
