# Claude Code for global and project supervisors

## Outcome

An owner chooses an Anthropic/Claude Code or OpenAI/Codex candidate for each supervisor conversation through its existing intelligence profile. A subsequent launch resolves the saved selection and records the installed adapter and executable in that generation. Conversation scope, history, dispatch identity and action receipts stay under AQ authority.

## Parallel implementation

Each row has a dedicated feature branch and worktree under the platform workspace's wt directory. The branch prefix is feature/claude-supervisor-. All branches target aq-server except ui, which targets aq-dashboard.

| Branch suffix | Owned changes | Acceptance |
| --- | --- | --- |
| controls | Profile discovery and save validation | Both routes agree on which harness, provider and effort combinations are supported. Existing selections resolve against the active snapshot. |
| host | Service config, admission, launch plans, descriptors | A generation records the selected executable digest and adapter. Legacy configs still launch Codex. A missing installation gives an actionable reason. |
| runner | Claude subscription turn and result handling | A verified binary receives startup input and supervisor instructions, returns a bounded reply, and uses the existing dispatch-bound publisher. |
| tools | Claude-accessible AQ commands, attachments, receipts and optional repository access | Tools enforce the existing scoped authorization. Agent-controlled input cannot execute an arbitrary local command. |
| fleet | Provisioning, manager configuration and upgrade procedure | Both installations can be selected in a retained conversation. An operator can upgrade a stopped scope and preserve its state and workspace. |
| ui | Dashboard validation, selection, fixtures and tests | Both global and project controls accept both harnesses and send the existing profile/candidate selection. |
| qualification | Switch and recovery regressions | A selection change affects a subsequent generation, preserving the scope and prior launch identity. |
| cli-check | Native CLI compatibility | Exercise the pinned CLI using isolated test inputs and verify the accepted flags and provider output shape. |

## Integration order

1. Agree on the optional Claude installation fields, the CLI version, and the supported effort values. Keep all existing Codex installation fields readable.
2. Build controls, fleet, UI and host in parallel. The runner and tools owners agree on the broker entrypoint and tool names. The qualification owner writes tests against the launch and control contracts.
3. Integrate backend branches together. Run formatter and diff checks, focused Rust tests against disposable PostgreSQL, fleet tests and dashboard contract/interaction tests. Include builds with the repository access features enabled.
4. Verify selection in both scope directions, unavailable installation behavior, credential redaction, action receipts and interruption handling. Record the distinction between fixture coverage and a live authenticated provider turn.
5. Roll out compatible UI and backend revisions together after review. On a designated test deployment, exercise one actual action per harness, switch an existing conversation and inspect its next reply and history before upgrading the fleet.
