# Agent result review reads v1

`POST /v1/agent/results/list` and `POST /v1/agent/results/show` let a reviewing attempt read its own project's retained results. See [`routes.json`](routes.json). Both need `result.read` at project scope (`project:self`, which the `aq.coding` default holds, or the attempt's own `project:<id>`); the project always comes from the credential's binding. `aq commands` shows them as `result.review.list` and `result.review.show`.

- They read through the same validated owner reads as the human result routes (`profiled_execution::human_results`), forced to the attempt's project. A report of another project is `not_found`, never revealed.
- A list isolates each report: an unreadable one appears in `unavailable` and `complete` is `false`, while the healthy ones are returned.
- `show` takes the exact report digest (`revision`). A correction is a different report; the original stays addressable.
- `work` says where the revision's work is retained. For a delivery attempt that is the task branch in central aq-git: `delivery_pushed_results` (by `report_id`, not by assuming an artifact id equals the result id) gives the ref and head, and the attempt's frozen git grant gives the repository and base. `provenance.digest` binds revision, repository, base, head and ref. aq-git serves refs and name-status but not patch bytes, so `patch.served` is `false` and `patch.retrieve` lists the steps that read the exact diff from the retained head (an attempt holding a live git grant fetches the ref and checks `FETCH_HEAD` against the head).
- If no retained ref or source receipt is recorded, `work.state` is `missing` and says so. Nothing is recreated, approved or settled by a read.
