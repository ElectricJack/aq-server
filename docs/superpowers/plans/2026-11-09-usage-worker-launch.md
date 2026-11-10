# Dashboard usage meters and worker launch

Owner requested planning and implementation while away on 2026-11-09. This authorizes the implementation and local validation; no plan-approval pause is needed. Earlier profile-deletion correction remains part of the active work.

## Outcomes

1. Provider quota in the shared top bar shows actual account usage percentages, named windows, reset times and freshness. Unknown remains unknown; failed refreshes retain clearly stale readings. OAuth/login bytes never reach the browser.
2. Selecting a worker exposes Launch agent alongside its existing controls. The owner can choose the intended work/context, submit once, see startup status on that worker, then click the resulting agent to open its actual interactive terminal.

## Sequence

- Finish and qualify delete-to-router behavior first; preserve the owner's edited profiles.
- Inspect quota collection, supported provider metadata interfaces, command registry, worker admission, task/session creation, Swarm projection and terminal controls. Reuse those paths rather than creating a second launcher.
- Wire account quota collection for the actual connected subscriptions. Prefer supported Codex account/rateLimits/read and the pinned Claude SDK's structured usage interface. Normalize percentages/reset timestamps, preserve stale observations, bound refreshes and keep credentials on the host/trusted service boundary. Document experimental interface/version assumptions.
- Add a selected-worker launch form with project/context and bounded prompt as required by the existing execution path. Preserve the selected node identity through admission, require normal owner/CSRF permissions, and record a recoverable operation identity. Show clear pending/start/failure state; uncertain submissions reconcile before retry.
- Make the started agent appear under the correct worker; clicking it must select the existing AgentPane terminal with the real session/terminal IDs. Reuse the current terminal ownership/control implementation and preserve watch-only behavior.
- Test the backend boundaries and UI flows, build clean bundles/native binaries, deploy only the isolated AQ server/dashboard, and verify desktop/phone images and the exact served bundle. Keep owner services and Supabase intact. Draft PRs remain subject to independent review; no merge without it.

## Acceptance

- Real quota readings and reset timestamps from connected accounts are visible; mocks are labelled, not live evidence. No invented zeroes or leaks of account tokens. Meter reads do not generate model output.
- Launch targets the selected eligible worker, does not duplicate work on double-click or lost responses, and reports terminal startup truthfully. Node availability/capacity and project permissions stay authoritative.
- Clicking the new agent opens its live terminal; desktop and 390/320px views remain usable, keyboard accessible and free from whole-page horizontal overflow.
- Meaningful targeted tests pass; Rust tests run under timeout in private per-worktree targets, existing integration binaries are reused, and server line ratchets only decrease. Runtime smoke is bounded and respects actual account usage. No unrelated cloud/swarm controls or Rust build-speed work is included.
