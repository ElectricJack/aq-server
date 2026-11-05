# A3b aq-git Release Pin Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task-by-task inline in the assigned AQ worktree. The task already authorizes execution and publication; follow AQ's completion protocol.

**Goal:** Ship the exact A3a aq-git v0.2.0 executable in both consumer images, with independently verifiable source provenance.

**Architecture:** Retain A3a's downloaded source archive, executable and build record without rewriting their bytes. Construct a complete Git bundle from the recorded release commit, inventory the archive and compare it to the bundle. Both images copy the executable through a digest-pinned stage that verifies SHA-256; Git 2.56.0 remains built from the existing retained archive.

**Tech Stack:** Python 3 standard library, Git bundles, Docker BuildKit, Rust 1.96.1, PostgreSQL 17.

**Spec:** A3b in `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml`; assigned task eager-grove-45.3.

## Global Constraints

- Each repository builds and tests alone. No sibling path dependencies, submodules or floating branches in release inputs. Rust uses the committed lockfile with `--locked`.
- No wire, authority, networking, hosting, stop-proof or secret-handling changes are in scope.
- Do not claim hostile-code isolation for shared-kernel containers. Unsupported required capabilities refuse explicitly.
- Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked --workspace`; record exact commands, environment, commit and limits in `docs/implementation/A3b.md`.
- Publish through guarded `aq git push`; never merge the default branch. A3b is direct integration and is not marked for adversarial review.
- No v2 binary is installed as `aq` or `agent-queue`. Named tests must actually pass; zero matches are unmet.
- Preserve Git 2.56.0 archive digest `26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3` and builder `rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663`.
- Do not edit non-aq-git base pins, state-format labels, Compose supervision or candidate/publish integration.

### Task 1: Verify and retain released inputs

**Files:** `scripts/test-git-candidate.py` (new behavior tests); `scripts/check-git-candidate.py`; `vendor/git-candidate.json`; `vendor/git-candidate.sha256`; `vendor/aq-git-source.{tar.gz,bundle}`; `vendor/aq-git-v0.2.0-linux-x86_64`; `vendor/aq-git-0.2.0/{source-inventory.json,build-record.json,git-2.56.0.tar.xz}` and retained Git checksum provenance.

**Interfaces:** Consume release commit `e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a`, archive digest `f64891f03e2d28263cf3af20b38123935ae08d6917699becb9d77e76af2fd3c6`, executable digest `3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2` from A3a's build record. Produce a self-contained candidate manifest with binary, inventory and build-record paths/digests.

- [x] Add tests that run the real checker against retained inputs, then copied fixtures with one byte appended to the executable, a changed build-record source revision, an archive traversal entry, an inventory mismatch and bundle/source disagreement. Verify official override refusal and Docker stage drift refusal.

```python
def test_changed_binary_refused(self):
    binary = self.root / 'vendor' / self.manifest['binary']
    with binary.open('ab') as stream:
        stream.write(b'changed')
    with self.assertRaisesRegex(ValueError, 'binary digest mismatch'):
        self.checker.candidate(official=True)
```

- [x] Run `aq test scripts/test-git-candidate.py`; observe missing binary validation before implementing.
- [x] Copy byte-verified A3a downloaded assets; create bundle using `git bundle create <absolute-path> HEAD` in the clean detached release checkout. Compute and record bundle SHA-256. Rename only the aq-git vendor directory, preserving Git bytes.
- [x] Implement candidate checks for safe retained paths, archive prefix/commit/version/lockfile, complete inventory, build-record provenance and matching bundle committed files. Continue rejecting local official overrides.
- [x] Run behavior tests and `python3 scripts/check-git-candidate.py --official`.

### Task 2: Embed verified executable and qualify consumers

**Files:** `scripts/Dockerfile.aq-git`, `Dockerfile`, `Dockerfile.worker`, `scripts/test-project-bindings.py`, `docs/source-retention.md`.

**Interfaces:** Consume manifest binary digest. Produce identical canonical builder blocks and images with that executable digest; update the synthetic project fixture's immutable image ID.

- [x] Replace the aq-git source build with verification and copying of the retained release binary. Keep source-archive digest verification in the stage and preserve the existing output path to consumers.

```dockerfile
COPY vendor/aq-git-v0.2.0-linux-x86_64 /src/target/release/aq-git
RUN echo '3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2  /src/target/release/aq-git' | sha256sum -c - && chmod 755 /src/target/release/aq-git
```

- [x] Generate both marked Dockerfile blocks from canonical text before the third `FROM`; run checker and drift behavior test.
- [x] Build standalone qualification image, server and worker. Supply worker source revision/archive digest from `vendor/worker-candidate.json`. Save build logs, image IDs and embedded aq-git/Git versions/digests. Update `GIT_IMAGE`/`GIT_IMAGE_ID` to the new standalone image.
- [x] Extract aq-git and the relocatable Git runtime from the new image into task-owned temporary storage. Start disposable PostgreSQL with `scripts/test-db`.
- [x] Run source_retention and project_bindings Rust suites with `--locked`, including the ignored source retention qualification using `AQ_TEST_GIT_EXECUTABLE` and `AQ_TEST_AQ_GIT_BIN`. Inspect the test's actual variable names before invoking.
- [x] Run `DATABASE_URL=... python3 scripts/test-project-bindings.py`, ensuring its named ignored public binding test passes. Keep all resources synthetic and let the wrapper clean up.
- [x] Update the reproducible package documentation with provenance, direct-copy choice, digest verification and qualification limits.

### Task 3: Definition of done, evidence and publication

**Files:** `docs/implementation/A3b.md` and this plan's checklist.

- [x] Run format, strict Clippy and the required locked workspace test command with bounded jobs/test threads; heartbeat before long foreground commands and retain logs.
- [x] Reconcile all A3b requirements, record commands/results, image digests, commit identity, source/tag/build-record evidence and remaining limits.
- [x] Commit implementation, squash own unpublished implementation history onto its recorded source base preserving the tree and attribution. Merge `origin/main` (supervisor waiver for absent `origin/dev`) into the task branch, resolve with context, run only targeted checks after the merge, and guarded-push the final SHA.
- [x] Record complete no-fail-fast results and all 32 exact branch/main comparisons. Full workspace acceptance has an explicit supervisor-approved exception (`msg-ca59a3e2ee0142fda8c632fbd677fb97`): 19 persistent failures reproduce identically on clean main, 12 cases pass unchanged isolation/rechecks, and one Graph case passes with its fixture configured. Final AQ close must carry that exception and the PR #19 dependency; its receipt and subsequent `aq session drain-ack` are recorded by AQ.

Current integration dependency: supervisor message
`msg-41efaa94f6ce40f093a85cb3b25549b1` explicitly authorizes stacking A3b on
`aq/eager-grove-45.3.3` (PR #19). That genuine historical-fixture prerequisite
was merged into A3b as `46fb7a19`. Strict Clippy, fmt, official verifier, image
builds and named A3b qualifications pass. Complete isolated `granted_closure`
passes 10/0. Full workspace no-fail-fast completes 225 target/doc-test results:
1,393 passed, 32 failed, 30 existing ignored. Every failure is reconciled by
clean-main comparisons and unchanged serial rechecks; no persistent A3b-only
failure remains. The supervisor accepts the explicit workspace exception.
All per-target and per-case results are retained in `docs/implementation/A3b.md`.
A3b depends on PR #19 landing first; the human owns that PR merge.
