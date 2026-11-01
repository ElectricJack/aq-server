# Human actions and session usage: local preview qualification

**Earlier state34 checkpoint.** The [canonical state35 promotion](dashboard-canonical-state35-2026-11-01.md) supersedes this record's current-preview identity and Supervisor status. Its historical state34 fixture proofs and rollback details remain retained.

The development preview at **http://localhost:4188** now serves task creation in
Work, current-account notification inspection in Settings, and on-demand usage
in Agents' Session details. State format is 34. The actual served bundle digest
is `sha256:a384e31c82f84f9ee5565512bc4ecbfad483bed0aefeac4fc7829668a5952ac5`.
The [qualification record](dashboard-actions-preview-2026-11-01.json) pins the
binary, bundle, source, owner checks and actual browser results.

The combined dashboard suite passed **413 tests in 19.29 seconds**; typecheck,
production build and mock exclusion passed. Bundling took 548 ms for 28 assets.
Focused SQL/Graph/schema checks and affected strict Clippy checks passed.

## Delivered behavior and limits

| Surface | Qualified behavior | Remaining boundary |
| --- | --- | --- |
| Work | One exact immutable creation request, real Graph receipt/current record, reload lookup, task navigation without an obscuring composer, desktop/390px/320px layout | Editing, bulk creation and execution launch remain separate |
| Task operation access | Delayed responses cannot cross operation identity; 401/403 immediately removes the private composer even if bootstrap is offline | Response-loss recovery has actual owner and UI regression coverage; the positive browser case qualifies create/lookup/reload, not a fault-injected browser recovery |
| Notifications | Exact current recipient, persisted message/detail, no-route state, mobile Back/focus; Viewer self-access and foreign-recipient refusal | No external messages were sent; escalation/digest transport is not completed by inspection |
| Agents | Independently bound session usage read only while details are open; category subtotals remain partial and missing costs remain unknown | Canonical session has no accounting projection; nonempty fixture is synthetic, not a provider or invoice claim |
| Work/Knowledge/Overview | Actual canonical Graph queries, desktop/phone navigation and preserved page composition | Complete workflow explanations, aggregate review/results and full-page design gates remain open |

Actual task creation used a separate disposable Graph and AQ database. The final
browser created exactly one task in 933 ms, then the separate 320px/390px/1440px
read-only navigation check passed in 1,714 ms. Canonical promotion probes made no
domain mutations. Nonempty notification and usage checks used real owner APIs
over explicitly labelled disposable state. Screenshots were inspected against
references 02, 07, 08 and 11 and the shared Settings design; artifact paths and
hashes are retained in the paired dashboard evidence.

The old preview was stopped, its final state33 backup validated, and the new
server upgraded the existing preview database to state34. A restored copy had
already passed staging. The exact live bundle/capabilities were checked after
restart. Live Graph (16 reads), usage, Overview and Settings browser checks
passed with zero mutations or page errors. The previous binary and backup are
retained locally; rollback to the old binary requires restoring its state33
backup, since the old reader deliberately refuses state34.

This delivery does not enable production native execution. The real Codex
qualification found stock helper-path, shell-profile and plugin defaults that
require the separately specified named read-only image and confinement work.
Configured Decisions/Playbooks proofs remain separate from this preview's
unconfigured optional owners. Supervisor, interactive terminals, media/comments
and account quota remain unfinished.
