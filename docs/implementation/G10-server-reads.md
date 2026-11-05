# G10 server reads (from steady-cascade-45.11)

The G10 Agents operations views read `GET /api/v1/sessions/{id}/operations`
(contract `Response::SessionOperations`). Access is owner-only and needs complete
visibility. Other callers get 403. Bootstrap advertises the read as
`session.operations.read`. No migration was added: every fact comes from rows that
H2–H7 already write.

| # | Fact | Source reused | Field |
|---|------|---------------|-------|
| 1 | Stall rung, evidence, quarantine scope | H3 `stall_episodes.state` (`ladder::Episode`) + `stall_quarantines` | `stall` (scope is always `session_and_lineage`, as H3 quarantines both) |
| 2 | Semantic progress and its time | H3 `stall::evidence::PROGRESS_SQL` over `observation_events` | `semantic_progress.observed_at` |
| 3 | Wait observation time | H4 `agent_waits` snapshot (`aq_wait_engine::Snapshot`), max member `last_checked_at_ms` | `wait` |
| 4 | Nudge delivery and mailbox observation | H3 `stall_nudges` + `mailbox_receipts` / `mailbox_deliveries` | `nudges[]` (`source` is `stall_ladder` or `owner`) |
| 4b | `session.nudge` capability | Existing H3 action kind `session.nudge` (`agent_access/messaging/session_action.rs`) | bootstrap `session.nudge` |
| 5 | Waits inspection capability | Existing owner routes `/api/waits`, `/api/waits/{id}` | bootstrap `waits.read` |
| 6 | Lineage totals and rate revision | H2 `observations::budget::lineage` + highest `accounting.rate_cards` revision for the manifest's provider and model | `lineage_usage` |
| 7 | Terminal snapshot/replay key | H7 `terminal_sessions` identity and epoch. Replay is served by the existing `GET /api/v1/terminals/{terminal_id}/output` | `terminal` |
| 8 | Instruction provenance | Retained ProfiledV1 manifest: catalog, agent profile, modules, context sources (as digests) | `instructions` |
| 9 | Requested permissions | Retained manifest's requested and effective scopes | `requested_permissions`, `effective_permissions` |

Manifest credentials (SecretRefs) are not projected.

Test: `cargo test -p aq-server --test profiled_results operations::`.
