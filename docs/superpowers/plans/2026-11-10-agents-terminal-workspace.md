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

Update this record with delivered commits and qualification outcomes as work lands.
