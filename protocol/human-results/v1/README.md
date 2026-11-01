# Human retained-result metadata v1

Mounted only on the human listener by the product integrator. Current Owner authorization is required; session inspection ACLs do not grant this installation-wide result permission. Every read rechecks the current Owner row in the same transaction as the retained-result owner reads.

- `GET /api/human/results?project_id=<UUID>&after=<report UUID>&limit=10`: optional project filter, stable UUID keyset, default 10 / maximum 25 reports. UUID order is not chronological Latest. `next_after` is a position only, never an access credential. A current Owner may inspect all projects; each report's exact project is validated by the result owner.
- `GET /api/human/results/<report UUID>/revisions/<report SHA256>?project_id=<UUID>`: one exact immutable report revision. Wrong digest/project gives not found. A correction has a distinct report UUID and `supersedes`; the original remains addressable.
- Both return `ResultPage` (Rust `profiled_execution::human_results` and TypeScript `api/adapters/results.ts`), schema version 1, server clock, explicit `recorded_legacy_native_only` coverage, items and optional cursor. Successful replies are `Cache-Control: no-store`.

The service calls `results::load_locked` for every exposed report. It validates the retained report/receipt, Started/Stopped/spec/outcome links and correction head. Metadata contains only project/task/attempt/report identities, exact report digest, recording time, worker conclusion/outcome, correction identity, repository/source **base** and worker-supplied source/effect reference IDs. It does not expose raw launch specs, credentials, worker paths or claim source receipts were verified by this metadata route. Process success is a worker-reported claim, not acceptance, settlement, publication or task completion.

Current phase-origin result publication remains unqualified. Empty means no retained reports in this supported coverage and selected scope, not no results in all AQ subsystems. A Stopped receipt without a result report produces no result card. Media, comments, feedback and state-changing controls are later owners.

At most two human result handlers execute concurrently, with a four-second total read timeout, SQL lock/statement deadlines and bounded pages. Reads use the general pool, not worker renewal/control pools. Per-report owner validation remains bounded work; there is no fleet or long-history throughput claim. Future larger feeds should batch validated owner projections rather than raise page limits blindly.

The paired metadata UI uses principal/auth-revision/scope-partitioned caches, at most four ten-row pages, explicit refresh/load-more and no automatic writes. Live capability `results.read@1` is enabled only when the admin mount and matching UI are assembled. Suggested root wiring: `.merge(profiled_execution::human_results::routes())`; never mount on the worker router.
