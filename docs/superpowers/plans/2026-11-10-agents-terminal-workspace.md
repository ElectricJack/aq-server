# Agents terminal workspace: implementation assignments

Owner-approved direction and detailed contract:
[`docs/specs/0009-agents-terminal-workspace.md`](../../specs/0009-agents-terminal-workspace.md).

The owner explicitly requested Terra implementation agents. Root dispatched the
following four `gpt-6.1-terra` agents with high reasoning, independent branches and
file ownership. No production deployment or merge into main is delegated.

| Task | Agent | Branch | Worktree | Initial status |
| --- | --- | --- | --- | --- |
| T1 server archive | `/root/terra_server_archive` | `feat/session-transcript-archive` | `wt/agents-history-server` | Dispatched; wire fixture confirmed |
| T2 worker capture | `/root/terra_worker_recorder` | `feat/manual-session-recording` | `wt/agents-history-worker` | Dispatched; source work first, coordinate Rust build disk use |
| T3 terminal workspace | `/root/terra_terminal_workspace` | `feat/agents-terminal-workspace` | `wt/agents-terminals-dashboard` | Dispatched; owns routes/shell and terminal panes |
| T4 history reader | `/root/terra_history_reader` | `feat/agents-history-reader` | `wt/agents-history-dashboard` | Dispatched; owns history feature/client |
| T5 integration and qualification | `/root` | Integration branches after task commits | Separate reviewable worktrees | Depends on T1–T4 |

T1 and T2 share section 6's additive, enrolled-node upload protocol. T1 supplied
the canonical hello/newline upload, transcript and search fixture to T2/T4 before
implementation. T3 registers T4's lazy route export; T4 does not edit shared shell
or route files. The spec branch starts at server `eef68854`; worker source starts
at `ab51f2d` merged with packaging `0a295b5`, with latest tested packaging retained
in the five conflicting installer/documentation paths. Dashboard tasks start at
`3a9614a`.

Each assignment includes focused tests, a commit, explicit limitations and evidence.
Desktop/phone rendered screenshots and actual transport qualification are separate
from component/API tests. All agents must preserve the owner's running laptop
session and protected services. No time estimates are assigned.

## November 11 recovery and integration

The interrupted root session is `01a1c24f-8051-71e0-8755-30fe44fc2cb9`
(November 9 title: Review agent-queue rewrite next). Its worktrees and commits
survived the Windows restart. Integration continued from a new session.

| Task | Delivered implementation | Qualification |
| --- | --- | --- |
| T1 | `feat/session-transcript-archive` through `a17106c5` | Full manual-agent SQL/router/WebSocket module: 7 passed; 2 opt-in worker integration tests passed separately; scoped Clippy, formatting and budget checks passed |
| T2 | `feat/manual-session-recording` through `29415877` | 10 recorder tests; actual production Docker capture and exact raw cap; real-server upload, finalization and corrupt-state partial recovery passed |
| T3/T4 | `feat/agents-terminal-workspace` through `43552ac`, including T4 through `b5beff1` | Integrated reader and panes; full unit baseline plus 51 final focused tests; typecheck/lint/build; 5 compiled-preview browser tests and all served asset hashes passed |
| T5 | Root integration and reviewed fixture evidence | Desktop/tablet/phone/320 px screenshots reviewed; no deployment or main merge |

The final dashboard supersedes `feat/agents-history-reader`; do not merge that
separate T4 branch again. New fixes cover stale saved panes, ended output,
protected-read metadata, asynchronous search races, response identity/scope,
phone expanded switching and clipped touch controls. Worker state loss now uses
the existing authenticated conflict cursor to finalize acknowledged output as
partial without discarding local pending chunks or inventing complete coverage.

See [the recovery handoff](../../handoff/2026-11-11-agents-terminal-workspace.md)
for exact paths, repeatable checks, remaining limitations and rollout boundaries.
