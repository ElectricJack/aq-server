# API-13 bounded task contributions

This increment adds `GET /api/human/overview/tasks` under the existing human Graph router. The [versioned contract](../../protocol/human-overview-tasks/v1/README.md), schema and labelled static example are separate from frozen dashboard-read-1.

A current Owner with a configured Graph Runtime can inspect bounded task lifecycle observations for selected configured projects. Complete observations, lower bounds and unknown/unavailable sources remain distinct. Graph active state is not runtime readiness or a running process. Graph's default collection excludes retired/deleted/disputed records; these are not global counts over every historical record. Projects sharing a workspace can show the same tasks, so no unique global total is summed.

Graph requests have a 3-second aggregate budget and at most 750 ms per project. Defaults are four projects and sixteen tasks per project, capped at eight/thirty-two. Individual source failure preserves valid other project contributions. The final human/project binding fence is reused and now checks all selected bindings in one ordered batch SQL query. No locks cross Graph HTTP, and no task row causes a SQL query.

Validation:

- Existing independent pinned Graph + isolated PostgreSQL route fixture extended with actual task counts, lower bounds, project pagination, mixed unavailable sources, schema checks and Owner revocation while an actual Overview read was held: two cases passed in 2.01 seconds, including prior task/knowledge regressions.
- Two pure parser cases passed in 0.01 seconds: task lifecycle versus execution semantics, metadata/body exclusion, lower bounds, future states, foreign workspace, duplicates and bounds.
- Affected library/route test strict Clippy passed in 27.08 seconds; owned formatting and diff checks passed.

The independently supplied Graph binary SHA256 remains `eb9c8f41002cb5d9675f87de0cadbe34e7d74cfc60d2449cd23ac4df76921fea`. Actual tests require `DATABASE_URL`, `AQ_TEST_GRAPH_BINARY`, and `AQ_TEST_GRAPH_SHA256`; the Graph fixture is explicitly ignored unless that environment is provided. Compilation is reported separately from test duration.

Root integration publishes `human.overview.tasks.read` v1 only after pairing UI-15 and mounted source, then checks the actual server/bundle. This source increment does not itself enable that capability or qualify the live preview. Review/result aggregation, quota, scheduling readiness and full Overview completion remain separate work.
