# Agents terminal workspace and retained session history

Date: 2026-11-10. Status: implementation authorized by the owner.
2026-11-13: aligned with the rolling-records principle ([architecture 0006](../architecture/0006-records-are-rolling.md)); implementation pending. §5.2 and §5.3 changed: the newest output is kept, old output and old sessions are deleted.

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

History indexes manual and native sessions. Manual Claude Code and Codex
sessions can have saved conversations. Native queue sessions retain their existing
inspection and live terminal links.

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

Keep an ended or failed agent's pane in place with input disabled and the last
observed live screen. Provide History and Close actions. A transient disconnect
uses the terminal's reconnect path.

## 5. Saved conversations

The worker reads the provider's native session log after the manual sandbox has
stopped. Extract bounded user and assistant text from the original sandbox's
root session. Apply credential masking before persisting the snapshot. The
worker can recover existing sessions while their native logs are available.
It retries the same snapshot after an uncertain upload.

The server binds uploads to the original enrolled node and stores the conversation
independently of the running worker. Saved messages remain readable after a
worker disconnect or server restart.

## 6. History contract

| Route | Access and response |
| --- | --- |
| `GET /api/manual-agents/history` | Owner, paginated session metadata including whether a conversation is available |
| `GET /api/manual-agents/{id}` | Owner, session metadata for a pinned terminal pane |
| `GET /api/manual-agents/{id}/conversation` | Owner, saved conversation or an empty result, bound to session and project |
| `POST /v1/worker/manual-agents/{id}/conversation` | Original enrolled node, immutable bounded conversation snapshot with replay acknowledgement |

## 7. History user interface

Filter sessions by project, agent, provider, model, dates, or state. Open the
selected conversation with user and agent labels and Find, Copy, and Download
actions. Show a partial-recording label when the worker reports incomplete
coverage. Show an empty state when a conversation is unavailable and provide
the inspection link for native queue sessions.

## 8. Implementation ownership

The server owns the History index and conversation storage. The worker owns
native log selection, parsing, masking and upload. The dashboard owns the
History reader and terminal workspace. The live terminal transport has its own
connection and input lifecycle.

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
5. End a manual session with native user and assistant messages and verify the
   conversation remains available after worker disconnect and server restart.
6. Verify exact retries, conflicting uploads, and wrong or revoked node access.
7. Page older sessions and distinguish an empty conversation from a refused
   read. Verify project scope on the list and selected conversation.
8. Find and export native messages without acquiring terminal control.
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
identity, native-session retention limitations and any deferred differences.
