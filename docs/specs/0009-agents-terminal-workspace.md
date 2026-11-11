# Agents terminal workspace and retained session history

Date: 2026-11-10. Status: implementation authorized by the owner.

The owner approved the proposed Terminals / Swarm / History direction and asked
for this implementation specification followed by implementation by Terra agents.
Owner requirements are marked **(owner)**. Concrete defaults chosen to make the
approved direction implementable are marked **(implementation decision)**.
These defaults may be refined without changing the owner requirements; document
material contract changes before another agent implements against them.

## 1. Outcome and scope

**(owner)** Agents is primarily the place to connect to running agents. A narrow
agent list and up to four interactive terminals should reproduce the useful v1
workflow. Past sessions remain discoverable, scrollable and searchable.

**(owner)** Put Agents directly below Supervisor in the application navigation.
Desktop order: Overview, Supervisor, Agents, Work, Results, Decisions, Playbooks,
Knowledge; Settings and identity remain at the bottom.

**(owner)** The Agents views are Terminals (default), Swarm, and History. Preserve
the compact header, automatic interactive input and existing provider/profile
selection. No desktop keyboard toolbar, watch/edit selector or launch-task prompt.

**(implementation decision)** Ship a complete vertical slice for the persistent
Claude and Codex agents launched through the manual-agent service. Include native
queue sessions in the agent/history lists using the existing inspection APIs and
open their existing qualified terminals when available. Native sessions without a
retained transcript must say so. Do not present a volatile tail as saved history.
Extending durable capture into any native executor that lacks it is a separate,
explicitly reported follow-up, not something this increment claims to deliver.

There is no drag-and-drop layout designer, synchronized input, video-like replay,
new orchestration engine, provider-specific conversation database importer, or
resume/relaunch behavior in this increment. No API-key provider integration.

## 2. Existing implementation and constraints

The inspected implementation bases are:

| Repository | Worktree / revision | Relevant existing implementation |
| --- | --- | --- |
| Server | `wt/dashboard-usability-server`, `eef6885442cbae766c876383bffdc00f7e791aed` | `crates/aq-manual-sessions`, migration 112, `tests/it_server/manual_agents.rs` |
| Dashboard | `wt/worker-bootstrap-dashboard`, `3a9614a0a7d452ba598a6db06b8708c1bda6e549` | `Agents.tsx`, `InteractiveTerminal.tsx`, `manualTerminalClient.ts`, `swarm/manualAgents.tsx` |
| Worker runtime | `wt/worker-bootstrap-worker`, `ab51f2d` | `aq-worker/manual.rs`, `manual/provider.rs`, `manual_stream.rs`, `aq-node-service/link.rs` |
| Worker packaging | `wt/worker-bootstrap-tmux-worker`, `0a295b5d6f015b6666b30d9a1e1510efe10b7e1d` | installed terminal-relay companion and remote receiver fixes |

The current manual-agent fleet read returns at most 256 records and only the last
day of stopped sessions. It is not an archive index. Its in-memory terminal Screen
and the browser's 64 KiB output tail are disposable. The worker's periodic
`capture-pane -S -500` is a screen/history snapshot, not a complete recording.
The dedicated WebSocket/PTY relay supplies responsive live input and output;
retain it. Each agent has its own controller lease and connection identity.

V1 is read-only reference material under `/home/jkern/dev/agent-queue2`; do not edit
or restart it. Leave Supabase, `aq-worker@local.service`, owner credentials and
unrelated project pools alone. Do not take over, stop or submit commands to the
owner's running laptop agent `bd829630-7e0e-453e-970b-de85c003c009` for testing.

## 3. Navigation, URLs and scope

**(implementation decision)** Routes:

| Route | Behavior |
| --- | --- |
| `/agents` | New terminal workspace |
| `/agents/swarm` | Existing graph and worker controls |
| `/agents/history` | Archive list; selected session is URL state |
| `/agents/sessions` | Compatibility redirect to History, preserving scope |
| `/agents/:sessionId` | Preserve legacy session detail links and inspection |

Register specific routes before `:sessionId`. Update internal Swarm links (worker
launch, graph selection, settings, overview) to `/agents/swarm`, including their
selection and terminal-focus query parameters. Translate old `/agents?select=...`
graph links to Swarm rather than silently losing the selected worker/agent.

Preserve the existing `scope` query parameter throughout. The terminal roster
contains agents in the selected project, or all authorized projects in All
projects. Scope changes remove out-of-scope live attachments; they do not stop
agents. Preserve pane choices separately per principal and scope. Store IDs and
layout preferences only, not terminal output, tokens or session content, in web
storage. Logout and authorization changes clear protected content/connections.
Preserve the existing compact phone primary-navigation arrangement unless a
separate ordering change is necessary to keep the approved desktop order clear.

## 4. Terminal workspace

### 4.1 Agent list

**(owner)** Left column on desktop, compact searchable rows. Show agent/project
name, lifecycle/connection state, model/provider and worker name without expanded
metadata cards. Include starting, running and stopping agents. Keep selected ended
agents accessible until their panes are closed. Include both manual and native
sources without confusing identical raw IDs; client keys are `manual:<uuid>` and
`native:<session-id>`.

Click a row to open/focus it. An accessible add/remove control toggles membership
in the terminal workspace. Selecting an already-open agent focuses its pane.
The maximum is four; at the limit explain that a pane must be closed to add
another. Never silently evict an existing pane. Show a concise empty state and a
Launch agent action using the existing project/worker/profile flow. Launching
creates a pending row/pane immediately, then attaches to that same session when
ready. Swarm retains its launch and slot controls.

### 4.2 Layout and actions

**(owner)** One agent fills the output area. Two agents use equal side-by-side
panes. Three use a two-by-two grid with one empty cell; four fill the grid. Do
not render a large decorative empty card in the fourth cell. Each pane has one
compact line of identity, connection state, Expand/Restore, Close, and an explicit
Stop action/menu. Closing a pane disconnects its attachment only. Stop uses the
existing lifecycle operation and does not imply deleting history.

**(implementation decision)** Keep readable terminal text and use the existing
terminal's horizontal panning when a tiled pane is narrower than its minimum
column width. Never shrink font to fit four panes. At widths below 1000 CSS px,
show one selected terminal with a quick switcher retaining up to four selections;
the narrow roster opens as a sheet/drawer. At phone widths use the existing touch
input controls and keyboard-aware sizing. At 320 px there must be no page-level
horizontal overflow, and there must be a visible exit from expanded terminal mode.

Terminal space dominates the page. Keep the global quota strip and compact Agents
header; no duplicated large title, description, status card or desktop key buttons.
Expanded mode temporarily shows one pane; restoring it preserves every selection.

### 4.3 Connection lifetime and focus

Key each mounted terminal by source/session identity. Grid changes, focus changes,
roster polling, row search and unrelated agent updates must not remount or
reconnect an existing pane. Each pane owns an independent connection, lease,
ResizeObserver and renderer. There is no shared singleton current terminal.

All visible running panes stay connected. Only the keyboard-focused pane receives
input/paste. Focus has an unmistakable border/header state, including keyboard
navigation. Opening the roster or searching history must not send those keys into
a terminal. Do not focus a newly arrived agent automatically while the owner is
typing elsewhere. A hidden pane in phone/focus mode must not send zero-dimension
resizes; either retain its last geometry/connection or deliberately suspend its
attachment without restarting its agent.

Retain bounded WebSocket backpressure, origin checks, per-session fencing, worker
ownership validation and no replay of uncertain keystrokes. A conflict with another
browser is local to that agent, with a clear reconnect action. It must not trigger
an automatic controller-takeover loop between devices or affect the other panes.

### 4.4 When an agent ends

**(owner)** Keep the pane in place, freeze input, mark it Ended/Failed, and retain
its observed output while switching to the saved transcript when available.
Transient network loss is Disconnected, not Ended. The archive can still be
finalizing after lifecycle stop; show that explicitly and refresh until settled.
Do not remove the pane when its agent leaves the active roster. Provide History
and Close actions. Never silently relaunch the process.

## 5. Durable recording

### 5.1 What is recorded

**(owner)** Newly launched manual agents record output even when no dashboard is
connected. Reading history must work after worker disconnect and server restart.

**(implementation decision)** Retain a chronological plain-text terminal output
transcript, not a promise of exact provider conversation JSON or pixel replay.
ANSI control actions, OSC clipboard/link payloads and terminal-device commands must
not execute in History. Full-screen redraws may produce repeated output, but new
text must not disappear just because the provider repainted its screen.
Label the view Terminal transcript. Do not infer prompts, roles or outcomes.

Capture is independent of tmux client attachment and terminal geometry. Use a
tmux output pipe (or an equivalent proven producer) established before the provider
starts; prevent the race where initial output is emitted before recording starts.
Do not keep a second visible tmux attach client that resizes the owner's terminal.
The recording pipeline must not make keyboard input depend on HTTP upload or SQL.

Use a bounded private worker spool, an incremental UTF-8/control normalizer and
the credential-shape protections already applied to terminal output. Handle split
UTF-8, split CSI/OSC, CR/backspace and credential prefixes across reads. Server
storage accepts only the sanitized text, never raw credential files or secret
material. Raw output needed locally must stay private and must not be printed in
logs, task messages or test evidence.

### 5.2 Ordering, retry and finalization

Worker emits immutable text chunks numbered from 1. Persist each chunk and its
upload progress atomically; retry the exact same chunk after an uncertain reply.
The server binds the upload to the original enrolled node and immutable session
identity. Identical retries return the same acknowledgement. A different payload
at an accepted sequence is a conflict. An out-of-order sequence is refused with
the next expected sequence; never silently concatenate a gap.

Use an ordered terminal finalization record after the final output has been
drained. Stop and natural exit drain capture before losing access to the sandbox;
if that is impossible, finalize with partial coverage and an intelligible reason.
Upload retries must continue for ended sessions even after manual polling removes
them from active jobs. The server must accept authorized late archive uploads for
stopped sessions. A revoked/wrong node remains refused. Worker and server restarts
resume from persisted identities and offsets; no duplicate text is created.

**(implementation decision)** Protocol bounds: each chunk at most 32 KiB of UTF-8
text; at most one upload per session in flight; 64 MiB retained normalized output
per session; worker raw spool at most 128 MiB per session. Bound line/chunk parser
state independently. On a size limit or capture loss, keep the existing transcript
and set coverage `partial` with a reason; never let a full archive block or kill
the interactive agent. No silent first-byte/tail truncation.

### 5.3 Retention and preexisting sessions

Keep archived normalized output until an explicit future retention/deletion policy
is approved. Do not attach these rows to the seven-day sealed-attempt cleanup or
the existing one-day manual roster filter. Closed transcripts are cold reads, not
resident runtime screens. Apply the size caps and paginated reads to bound work.

Existing sessions cannot acquire missing earlier history retroactively. If a safe
capture upgrade can attach to an existing session without restarting it, mark the
recording partial with its actual start time. Otherwise show Not recorded. Never
restart a live owner session to obtain a full recording or claim a screen snapshot
is a complete transcript. Metadata for old stopped manual sessions remains visible
with the truthful coverage state.

## 6. Shared server/worker/dashboard contract

**(implementation decision)** Extend `aq-manual-sessions` in focused modules; keep
the aq-server core ratchet at or below 108915. Migration 113 is the next available
migration at the inspected base. Add archive metadata and immutable chunk storage
with session/node/project ownership and indexed `(session_id, sequence)` access.
Keep archive identity/metadata after worker removal; revocation prevents writes,
not authorized owner access to already retained history.

New routes (all times are integer UTC milliseconds):

| Route | Auth and purpose |
| --- | --- |
| `GET /api/manual-agents/history` | Owner; paginated archive metadata, including old sessions without output |
| `GET /api/manual-agents/{id}/transcript` | Owner; bounded text chunks and archive status |
| `GET /api/manual-agents/{id}/transcript/search` | Owner; literal search over retained output |
| `POST /v1/worker/manual-agents/{id}/transcript` | Original enrolled node; append chunk or finalize |

History query: optional `project_id`, `status` (`active` or `ended`), `provider`,
`model`, `created_from_ms` (inclusive), `created_until_ms` (exclusive), `q` (metadata
substring, at most 200 characters), `cursor`, and `limit` (default 50, maximum 100).
Sort by descending `(created_at_ms,id)` using stable keyset pagination. It is
separate from fleet polling and must not increase the active endpoint's 256 limit.
Response: `{sessions:[HistorySession],next_cursor:null|string,server_time_ms}`.

`HistorySession` is the existing `Session` wire shape plus
`archive:{state,coverage,reason,first_sequence,last_sequence,bytes,updated_at_ms}`.
`state` is `not_recorded|recording|finalizing|complete`; `coverage` is
`unknown|complete|partial`. `reason` is nullable bounded plain text, not raw errors.
`complete` coverage is asserted only for a successfully drained recording from
provider startup. `not_recorded` has no invented chunks or timestamps.

Transcript query: `after` (exclusive sequence, default 0), `limit` (default 32,
maximum 64), and optional `through` high-water sequence to freeze a reader's
snapshot. Response: `{session:HistorySession,chunks:[{sequence,text}],next_after,
has_more,through_sequence,server_time_ms}`. Cap response text at 256 KiB even if
the count limit allows more. Unknown session: 404; known unrecorded session: 200
with empty chunks and `not_recorded`. Reads must not acquire terminal control.

Search query: `q` (literal, case-insensitive, 1..200 characters), optional `after`
(exclusive match byte offset; omit for the first page so offset zero is included),
`limit` (default 50, maximum 100), optional `through`.
Response: `{matches:[{offset,length,sequence,excerpt}],next_after,has_more,
through_sequence,server_time_ms}`. Offsets/lengths are UTF-8 bytes into concatenated
normalized text; excerpts are bounded plain text. Search must find a match crossing
a chunk boundary. Escape SQL wildcard syntax; do not accept regex. Bound query
work by the 64 MiB archive limit and server statement timeout. Pagination must
advance even on repeated matches. The UI converts byte offsets deliberately.

Upload body is either `{sequence,text}` or
`{final_sequence,coverage:"complete"|"partial",reason:null|string}`. A finalization
requires exactly the acknowledged high-water mark; it is idempotent. Response:
`{accepted_through,next_sequence,archive_state}`. Enforce body limits, strict JSON
fields, actual ownership, revoked enrollment checks and UTF-8 byte bounds. Include
`next_sequence` in out-of-order conflicts. No credentials in payloads or URLs.

Preserve existing endpoint shapes for old receivers, terminal relay and the live
dashboard. Recording is additive: old workers keep running with Not recorded
history. Capability/version negotiation or equivalent explicit unsupported handling
must distinguish an old server from an empty archive. Do not retry 404 in a loop.

## 7. History user interface

**(owner)** Search/filter the list by project, agent/session, provider/model, dates
and ended state. Reuse the global project scope. Filters supported only by native
metadata must not be invented for manual records. Manual and native records are
distinguished internally, not by making the owner choose a transport type first.

The selected session opens a substantial transcript pane, with one compact
identity/status line and Find, Previous/Next match, Copy and Download controls.
Keep session details on demand. Load bounded pages, preserve scroll position when
prepending earlier content, and avoid rendering an entire 64 MiB session into the
DOM. Search covers the whole retained session, not only the loaded page. Selecting
a match fetches the necessary context and scrolls to/highlights that match.
Display progress/error/cancel for a multi-page download; no partial export may be
presented as complete. Copy explicitly copies the visible selection or loaded text;
label its scope. Native sessions can use existing retained artifacts when present;
otherwise show the honest unavailable state and existing inspection details.

While an ended archive is finalizing, retain visible output and show Saving final
output. Partial/not-recorded banners are one compact line, with reason/details
available. Do not reset reading position or jump to the bottom on background
metadata refresh. Live terminal scrollback and archived transcript are separate
views of the same session, not separate agents.

## 8. Implementation task graph and file ownership

All implementation agents must be **gpt-6.1-terra**. Each uses an isolated branch
and worktree, reads its repository's AGENTS.md and this spec, and commits its work.
Do not merge into main or deploy/restart the owner's live services during these
tasks. Root owns integration, contract changes, review and final reporting.

| ID | Task / owner | Files and deliverables | Dependencies |
| --- | --- | --- | --- |
| T1 | Server archive / Terra | `aq-manual-sessions` archive APIs/storage; migration 113; real-SQL/router tests in existing `it_server` and schema tests; exact wire fixture | This spec |
| T2 | Worker recorder / Terra | `aq-worker/manual.rs`, new recording modules, `aq-node-service/link.rs`; private spool, capture-before-start, retry/finalize, restart/limit tests | This spec's wire contract; coordinate with T1 |
| T3 | Terminal workspace / Terra | dashboard routes/destinations, AgentsTabs, new terminal workspace, source-qualified roster, focus/layout/ended pane behavior; update legacy graph links | This spec; existing APIs, no T1 dependency for live panes |
| T4 | History reader / Terra | new dashboard history feature and API client/adapters, literal search/context/paging/export tests; no edits to T3-owned shell/routes/terminal files | This spec's wire contract; T1 integration fixture |
| T5 | Integration / root | integrate T1..T4, reconcile contracts, add browser scenarios, review desktop/phone screenshots, publish reviewable branches and task outcomes | T1..T4 |

T3 registers the lazy `/agents/history` route expected from T4; T4 exports its page
from `src/features/agents/history/index.tsx`. T3 owns `AgentsTabs.tsx` and shared
route changes. T4 owns only `features/agents/history/**` and
`api/sessionHistory*`. Both extend the existing native/manual adapters rather than
duplicating terminal transport. T1 publishes a fixture matching section 6 before
making incompatible changes. T2 may add focused protocol tests without introducing
a new integration executable. Additional work discovered is reported explicitly.

Resource constraints: disk is tight on this host. Use a private Cargo target per
worktree, never another worktree's cache, never delete another agent's artifacts,
and coordinate Rust compilation with root. Every Cargo test/nextest uses timeout;
iterate one existing binary/module, preserve incremental builds and line budgets.
No model invocations are needed for automated tests. Label fixtures as fixtures.

## 9. Required acceptance evidence

1. Desktop rail order matches the owner request. `/agents` opens Terminals;
   Swarm/History and old deep links preserve the selected project/session.
2. Open four independently identified agents, including different workers and
   providers in deterministic tests. Each receives only its own input. Search,
   polling, focus, grid changes and Expand/Restore do not reconnect other panes.
3. Close one pane and verify the same process/session remains running. Stop a
   disposable QA agent and verify its pane remains with ended output/history.
4. Connection loss freezes input without losing the last screen; reconnect uses
   the same agent. No uncertain input is replayed. Another device's controller
   conflict cannot cause an automatic takeover loop.
5. Record a disposable no-inference shell session with the browser closed. Emit
   more than the old 500-line/64 KiB limits. Stop it, disconnect the worker, restart
   the test server and retrieve/search beginning, middle and final output.
6. Retry chunks after a lost reply and restart the recording process: exactly one
   copy of each chunk remains. Test conflicting retries, gaps, wrong/revoked node,
   late final uploads, limits, partial capture, invalid UTF-8/control boundaries,
   and credential patterns split across chunks. Test search crossing boundaries.
7. History list reaches sessions older than one day and beyond 256 records;
   paging has no duplicates/omissions. A known unrecorded old session is distinct
   from unauthorized, missing, unavailable and genuinely empty output.
8. Find locates unloaded matches and navigates to them. Export includes every
   retained chunk in order or reports failure; reading does not claim control.
9. Inspect actual rendered desktop 1440 px and phone 390 px screenshots, plus
   320 px reflow and tablet transitions. Preserve compact chrome, readable text,
   touch controls, focus indication and visible exits. Review the existing Agents
   reference images `08-agents-focus-desktop.png`, `11-agent-mobile-v2.png` and
   `14-swarm-tree-v3.png`. The owner-approved grid/roster supersedes the old
   two-pane/permanent-roster restrictions; record this intentional difference.
10. Run focused dashboard tests, typecheck/lint/build and appropriate Rust tests,
    formatting and build-budget checks. Root checks the compiled preview's bundle
    and reviews evidence. Synthetic screenshots do not substitute for actual live
    transport testing. Production rollout and any runtime/installer pin change
    require their own reviewed delivery; do not disrupt the owner's current agent.

Record delivered scope, exact revisions/tests, desktop/phone images, served bundle
identity, native-session retention limitations and any deferred differences. Do
not call the entire history feature complete if only a volatile tail works.
