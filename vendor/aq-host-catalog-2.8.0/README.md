# AQ host catalog 2.8.0

The delivery lineage of catalog 2.7.0, so that a delivery playbook can tell
that publishing a result again is pointless: `work.read` now says which head
the pull request's checks were evaluated on.

## Changed

- `work.read` 2.8.0: `pull_request` adds `checks_head`, a string of at most 128
  bytes, or `null` when the host has not evaluated the checks of any commit of
  the pull request (just opened, or closed or merged by a command that did not
  look at its checks). It is the head `checks.state` was evaluated on. When it
  equals the result's `head` and `checks.state` is `failed`, the forge will not
  rerun the checks for that head, so publishing the same head again and waiting
  cannot change them. Every other fact is unchanged, and `pull_request` is
  still `null` until a pull request is opened.

  A 2.8.0 answer with a pull request does not fit 2.7.0 (it has no
  `checks_head`), so an artifact sealed on 2.7.0 or earlier keeps its own
  `work.read`; the capability stays `work.read`.

Every other contract of 2.7.0, and every event, is byte-identical.

## Files

`registry.json`, `manifest.json`, `fixtures.json` and `notes.json` follow the
shape of catalog 2.7.0's. The pin beside this directory records their digests.
