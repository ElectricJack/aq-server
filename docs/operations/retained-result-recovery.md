# Recover an exact retained patch for review

Use this when an independent review needs the exact source change a worker
retained, and the normal result read does not work for the reviewer. The
worked case is the activation review blocker (task `d9c43028`): result
`df3d2e44`, report revision
`44a9f35e560162b48354d5374ec9c9ccc419a757ec82b5e0b52e18dbddbe8f89`, central
repository `674b55a3-94ec-55b4-8338-df9c4fc8a0ed`, base
`957a001e392c4405b00ea1f11a1e621b19842447`, head
`105c94ad4a258ab7b943f14bbd31db935ec9af5e`.

This is an **owner** procedure. It reads retained objects; it does not
approve, merge, release a retention handle or change task state.

## Why the normal reads fail

Each symptom has its own cause. Do not treat them as one outage.

| Symptom | Cause | Evidence |
| --- | --- | --- |
| Reviewer: `result.show` / `result.detail` refuse `not_available`, "no route for this principal", although `result.read` is granted | `result.list`, `result.show`, `result.media.list`, `result.feed`, `result.detail` and `result.comments` have an owner route only. The registry gives a supervisor a route to an owner command only in a build with the `supervisor_full_access` feature (through the owner bridge), and gives an agent none. Without a route the grant has nothing to route to; a grant never adds one. | `crates/aq-command-registry/src/catalog.rs` (`result.*` specs have no `agent` route) and `lib.rs` (`supervisor_full_access` bridge); `aq commands` in an agent session lists no `result.*` command |
| Supervisor: `result.show` with `recorded_native_v2` or `project_id` returns "invalid result metadata query" | A native report is addressed by its **64-character lowercase hex report digest**, not by a small number such as `1`. The route validates that before reading. | `crates/aq-server/src/profiled_execution/human_results.rs`, `read_inner` (`revision.len() != 64`) |
| Supervisor: `result.detail` revision `1` is `not_found`; `attempt.show` says "attempt not found" | `result.detail` reads the feed store, a different store from native reports, and revision `1` is not a native digest. The attempt record is not in the store that route reads. | same file; `/api/v1/results/{id}/revisions/{revision}` vs `/api/human/results/{id}/revisions/{revision}` in the catalog |
| Supervisor: `result.list` with `recorded_native_v2` returns `conflict`, "registry refresh preparation unavailable" | A server-side readiness condition of the profiled registry, not a data problem. Report it; do not retry in a loop. | not reproduced here |

The authoritative revision of a native report is the digest the report
records (here `44a9f35e…e89`). Task state `finished`/`held` and an ended
session do not change it. Retained work is kept by central retention until a
release decision; none of the reads above release it.

## What is retained, and where

The worker checkpoints through aq-git, uploads the source and the server
obtains a central retention receipt before the report is recorded (see
[source retention](../source-retention.md) and
[architecture 0002](../architecture/0002-execution-and-recovery.md)). The
objects live in the central aq-git state directory, in the bare object store

```text
<state-dir>/repositories/<repository-id>/objects.git
```

`<state-dir>` is the `git_root` of `[source_retention]` (default
`/var/lib/aq/git`). The directory is private to the server's UID (10001 in the
packaged image), so run the commands below as that identity on the host that
runs central aq-git. An agent session has neither the directory nor an
installed command for it.

## Retrieve the patch

1. **Read the immutable identity.** Take repository id, base OID, head OID and
   report revision from the report as the owner reads it
   (`GET /api/human/results/{result_id}/revisions/{report_digest}`). Write them
   down; every later step compares against them. Never take base or head from
   a branch name.

2. **Check the objects are present and see the changed paths.** aq-git's
   `diff` operation lists paths and a status letter; it does not return patch
   text. It is the cheap proof that both OIDs resolve in the central store.

   ```sh
   aq-git --socket /run/aq-git/central.sock --json --input req.json diff
   ```

   with `req.json`:

   ```json
   {"protocol_version":1,"request_id":"retrieve-1","operation":"diff",
    "repository_id":"<repository-id>",
    "params":{"base":{"algorithm":"sha1","hex":"<base-oid>"},
              "target":{"algorithm":"sha1","hex":"<head-oid>"}},
    "deadline_ms":20000}
   ```

   The reply is `{"ok":true,"result":{"entries":[{"path":…,"status":"M"}],
   "truncated":false}}`. Compare the path list with the files the task says it
   changed. `truncated:true` means the list is incomplete; do not rely on it.
   An error `repository_not_found` or an unresolved OID means the retained
   source is not in this store: stop, and do not substitute another revision.

3. **Produce the exact patch from the retained objects, read-only.**

   ```sh
   G=<state-dir>/repositories/<repository-id>/objects.git
   git --git-dir="$G" diff --binary --full-index <base-oid> <head-oid> > patch.diff
   ```

   `--binary --full-index` keeps binary hunks and full blob ids, so the result
   is byte-exact and each `index` line names the blobs to compare. Use the Git
   that aq-git runs with (`/opt/aq-git-runtime/bin/git` in the packaged
   image), not a different version. The command only reads; do not run
   `fetch`, `gc` or `repack` in this directory.

4. **Verify before use.**

   ```sh
   sha256sum patch.diff              # must equal the digest recorded for the hand-off
   git apply --numstat --summary patch.diff   # file list and line counts, no repository needed
   ```

   For each file, the `index <pre>..<post>` line must show the pre-image blob
   present in the reviewer's checkout of `<base-oid>`
   (`git rev-parse <base-oid>:<path>` equals `<pre>`), and, after applying in a
   scratch copy, `git hash-object <path>` must equal `<post>`.

5. **Hand it over without a new route.** A patch is tens of kilobytes. Send it
   as a task message or comment body in compressed form, with the digest and
   the identity from step 1, so the reviewer can check it with only the
   installed `message`/`comment` commands:

   ```sh
   python3 -I -c 'import sys,zlib,base64; sys.stdout.write(base64.b64encode(zlib.compress(open(sys.argv[1],"rb").read(),9)).decode())' patch.diff
   ```

   The reader reverses it and checks the digest:

   ```sh
   python3 -I -c 'import sys,zlib,base64,hashlib; d=zlib.decompress(base64.b64decode(sys.stdin.read())); open(sys.argv[1],"wb").write(d); print(len(d), hashlib.sha256(d).hexdigest())' patch.diff < patch.b64
   ```

   Do the decode in a new empty directory, with the script outside it, and
   never run anything from the decoded patch. Compare the printed digest with
   the one stated in the hand-off before trusting a byte.

## Worked example (activation review)

The patch from task `d9c43028` decodes to **8857 bytes** with SHA-256
`7e2bd5e1a5642bea9da4bc5d76c56cd12b587740acf84e46910add3111f16d01`.
This was checked from the task bundle: the digest matches, and
`git apply --numstat --summary` reports two modified files and no binary hunks:

```text
29   8   src/mocks/humanGraph.ts
34   0   tests/work-activate-task.spec.ts
```

with blob ids

```text
src/mocks/humanGraph.ts           907bb4c1b240488c8fa7a499d610eafe524bdce0..819406f7cf009d10917da4aef5fda4e514e99c5c
tests/work-activate-task.spec.ts  c80174a9bacd8ab2b080647d2c0482cf68fed225..a3e8ea8cec9c7d479c93f0456c0bb20cdae80edf
```

Those two files belong to the dashboard repository, not this one, so the
patch could not be applied or its tests run from the aq-server workspace. A
reviewer applies it in a temporary checkout of the dashboard at base
`957a001e…` and runs the dashboard's own tests there.

## Permission and capability limits

- **Who can do this.** Only an owner with host access to the central aq-git
  state directory and an owner session for the report read. A reviewer or a worker
  agent cannot, whatever `result.read` grants it; a supervisor can read only
  where the installed build has the full-access owner bridge, and only by the
  digest revision.
- **What an agent session can read.** The installed command set (`aq commands`)
  has task, comment, message, knowledge and graph reads, and no result,
  attempt, session or repository command. An agent can read a hand-off
  message; it cannot fetch a result.
- **What the evidence proves.** The report digest, base and head identify the
  retained source. The SHA-256 proves the patch text equals what the owner
  produced. Neither proves the change is correct, the tests pass, or the
  review is done; the review stays pending until a reviewer has read the exact
  diff.
- **Not available yet.** A project-scoped result read route for agent and
  supervisor principals is owned by task `766481a0` (prerequisite AQ Git task
  `2b1c4588`). This page does not add it. When it lands, prefer it and keep
  this procedure as the fallback.
- **Retention.** The central handle keeps the objects until a release decision
  or the `keep_per_task` / `max_age` sweep of an eligible settled task
  ([settled task cleanup](../source-retention.md#settled-task-cleanup-sbx24)).
  A held attempt is protected. Retrieve and hand off before closing the task.
