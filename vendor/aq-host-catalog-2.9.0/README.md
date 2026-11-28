# AQ host catalog 2.9.0

The delivery lineage of catalog 2.8.0, so that a delivery playbook can read
which head's checks already failed. 2.8.0 put that head in
`pull_request.checks_head`, but `pull_request` is nullable and a playbook
reference cannot traverse a nullable value, so no playbook could read it (nor
`pull_request.checks.state`).

## Changed

- `work.read` 2.9.0: `facts` adds `failed_checks_head`, always present, a
  string of at most 128 bytes: the head of the result's pull request whose
  checks were last evaluated and failed, or `""` when there is none (no pull
  request yet, its checks never evaluated, or their last evaluation did not
  fail). It is `pull_request.checks_head` while `pull_request.checks.state` is
  `failed`. When it is not `""` and equals the result's `head`, the forge will
  not rerun the checks for that head, so publishing the same head again and
  waiting cannot change them. Every other fact is unchanged, `pull_request`
  included (it keeps 2.8.0's `checks_head`).

  A 2.9.0 answer does not fit 2.8.0 (it has `failed_checks_head`), and a 2.8.0
  answer does not fit 2.9.0 (it lacks it), so an artifact sealed on 2.8.0 or
  earlier keeps its own `work.read`; the capability stays `work.read`.

Every other contract of 2.8.0, and every event, is byte-identical.

## Files

`registry.json`, `manifest.json`, `fixtures.json` and `notes.json` follow the
shape of catalog 2.8.0's. The pin beside this directory records their digests.
