# Global and project supervisors: usable end-to-end service

## Owner scope

Owner requests completion from backend through frontend, and explicitly selected both global and project supervisors. A supervisor must accept chat, manage real project tasks through authenticated AQ commands, and expose progress/replies in the dashboard. Continue using subscription login only, the isolated aq-ms1 deployment and protected local worker/Supabase/v1 boundaries. Existing manual Claude/Codex agents and their terminal transport remain supported.

## Decisions (my call)

- Reuse the existing mailbox, reservations, generation/permit host, action journal and reply authentication. Connect these owners with an automatic central controller; do not fabricate a worker task, enrollment, reply or delivery event.
- One durable global conversation and one durable conversation per registered project. Preserve global identity/history. Explicit supervisor selection in dock/full-page UI changes recipient; changing presentation does not. Isolate drafts, reading state and uncertain sends by conversation.
- Project supervisors are constrained by server-derived project scope for reads, task/graph mutations and mail. Global supervisors can work across installed projects. Revocation, stopped heads, stale generations and cross-project actions fail closed. Expose only actually implemented supervisor commands.
- Add compact Start/Stop and saved Codex intelligence-profile controls plus actual idle/working/waiting/failure status. Logical message acceptance remains distinct from handoff and task completion. Preserve history/source on Stop; no silent replay after uncertain execution.
- The existing host runs an isolated, UID-separated Codex process with a private ChatGPT subscription auth copy, rather than requiring an API key. No provider material goes to workers or frontend. Each supervisor has a dedicated Docker container; the existing guardian owns its native lifecycle. Known CLI settings are generated, with no host config copied.
- Each turn binds an immutable mail batch, bounded prior-conversation context and profile snapshot before launch. The native wrapper publishes its actual final text through the generation/dispatch-bound reply writer while that generation is still active, avoiding a missing reply if the model does not invoke a reply tool. Preserve explicit tool replies without duplicating them.
- Tools use a scoped supervisor credential file and the existing command registry/action journals. Start with project/task read, create, graph create, edit, priority and dependency operations for ordinary supervision, preserving Graph validation and configured human review/delivery rules. Do not grant owner-only infrastructure/credential/configuration operations.

## Implementation and acceptance

1. Add scope/control storage and project conversations with immutable identity, original global mail proofs, current owner authorization and idempotent controls. Keep migrations additive.
2. Put new control/storage helpers outside the oversized server lib and move existing pure conversation metadata helpers to preserve/decrease its line ratchet. Add tests to existing binaries only.
3. Extend current supervisor auth/command routes with project scope and real read/write adapters; validate cross-project refusal at the route as well as command boundary.
4. Add central subscription adapter, a bounded AQ tool client, profile/plan assembly and a polling controller around the existing reservation/host lifecycle. Serialize each supervisor's turns, renew live permits, stop confirmed processes before successors and keep uncertain handoffs held.
5. Add the explicit supervisor selector, profile/actions/status to existing dock/full-page conversation, retaining existing message behavior and rendering verified task receipt cards and request identity fences. Update the product design for the owner's new scope requirement.
6. Focused SQL/router and physical host tests; UI tests for scope/drafts/send retries/controls; typecheck, fmt, lint, Clippy, budgets and builds. Inspect actual desktop/dock/phone captures against references 04/09/12.
7. Scoped live qualification: genuine global reply; separate genuine project reply; actual draft task created and visible on Work; scope refusal against another project; Stop/restart with history retained. Use our own QA messages/tasks, preserve owner history and work, and report limits honestly.
8. Deploy exact clean bundles/binaries into aq-ms1, verify served hashes and protected services, publish draft PRs for independent review, and record supervision/retrospective notes on main with [skip ci]. No merges without review.
