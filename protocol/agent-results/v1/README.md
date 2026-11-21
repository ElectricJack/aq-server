# Agent result review reads v1

`POST /v1/agent/results/list`, `POST /v1/agent/results/show` and `POST /v1/agent/results/patch` let a reviewing attempt read its own project's retained results. See [`routes.json`](routes.json). All three need `result.read` at project scope (`project:self`, which the `aq.coding` default holds, or the attempt's own `project:<id>`); the project always comes from the credential's binding. `aq commands` shows them as `result.review.list`, `result.review.show` and `result.review.patch`.

- They read through the same validated owner reads as the human result routes (`profiled_execution::human_results`), forced to the attempt's project. A report of another project is `not_found`, never revealed.
- A list isolates each report: an unreadable one appears in `unavailable` and `complete` is `false`, while the healthy ones are returned.
- `show` takes the exact report digest (`revision`). A correction is a different report; the original stays addressable.
- `work` says where the revision's work is retained. For a delivery attempt that is the task branch in central aq-git: `delivery_pushed_results` (by `report_id`, not by assuming an artifact id equals the result id) gives the ref and head, and the attempt's frozen git grant gives the repository and base. `provenance.digest` binds revision, repository, base, head and ref. `show` carries provenance only (`patch.served` is `false`); `patch.retrieve` lists the fallback steps that read the exact diff from the retained head (an attempt holding a live git grant fetches the ref and checks `FETCH_HEAD` against the head).
- If no retained ref or source receipt is recorded, `work.state` is `missing` and says so. Nothing is recreated, approved or settled by a read.

## Exact patch (`result.review.patch`)

`patch` takes the same `report_id` and exact `revision` as `show` (another project's report, an unknown report and a revision that is not the report's digest are `not_found`) and optional `max_bytes` (1..4194304 raw bytes). It binds the revision to the base, head and root repository AQ recorded and asks central aq-git for `diff.patch` between those two typed commit OIDs. The answer is returned only after its `format` (`git_diff_binary_full_index_v1`), `base` and `target` OIDs, size bound, decoded length and `sha256` were checked; the patch is complete or not served. `patch.value` is standard padded base64 of exactly `size` raw bytes; `patch.sha256` is lowercase hex over those bytes. Nothing is cached, recreated or approved, and a report with no retained work answers `patch.served: false, reason: no_retained_work` with `work` as the evidence.

Operator requirement: central aq-git must offer the `diff.patch` operation and `limits.diff_patch_bytes` in `capabilities` (aq-git PR 24 or later; check `aq-git --socket <central.sock> --json capabilities`). An older aq-git yields `patch.served: false, reason: unsupported` and the `retrieve` fallback; nothing else about the other routes changes. Until the service is upgraded, no patch is served.

Reading holds one aq-git exchange of up to 20 s and one patch at a time server-wide (a second concurrent request is `503 overloaded`); the response can reach about 5.6 MiB.
