# Subscription usage and selected-worker launch

The owner requested these MVP controls on 2026-11-09. This increment adds account quota and manual selected-worker tasks; it does not enable paused projects or add cloud provisioning.

## Account usage

Run `scripts/refresh-provider-usage.py --output /absolute/path/usage.json --claude-sdk /absolute/path/sdk.mjs`. Install/pin the official Claude Agent SDK separately; this adapter is qualified against 0.3.321 and CLI 2.1.321. Codex uses its supported `app-server` `account/rateLimits/read` method. Claude uses the SDK's explicitly experimental `usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET` method. No prompt is emitted, no model inference is requested, no login file is parsed, and API keys are excluded. Claude login must support plan/profile metadata; an inference-only setup token cannot supply quota.

The helper publishes percentages, window labels, reset times and actual observation times only. Errors produce a short source status, retain a previous observation with its original timestamp, and never invent zero usage. Run the helper every minute using the host's service manager. Keep raw login material outside the server container. Mount only this sanitized directory read-only. The output file is readable by the container UID; the parent deployment directory should remain private.

```toml
[[provider_quota]]
provider_id = "anthropic"
account_ref = "owner-claude-subscription"
account_label = "Claude"
snapshot_file = "/var/lib/aq/provider-usage/usage.json"

[[provider_quota]]
provider_id = "openai"
account_ref = "Codex ChatGPT"
account_label = "Codex"
snapshot_file = "/var/lib/aq/provider-usage/usage.json"
```

Snapshots expire after two minutes. Account usage can be configured even when that account is not yet qualified for agent execution; this confers no model/credential readiness. Existing protected Codex OAuth-file quota configuration remains supported. The header's account labels, percentages, reset times, all-window disclosure and stale rendering use the existing dashboard contract.

## Launch and terminal

Select an online worker in Agents. Launch agent lists enabled projects only. Enter the work to perform, then launch. The task uses the project's configured agent/model/source policy. Paused projects remain paused. A full worker queues the request for that worker's next available slot.

`GET /api/human/workers/{node}/launch-projects` returns allowed project identities/current revisions. `POST /api/human/projects/{project}/workers/{node}/launch` accepts the existing `human-task-create-1` request with `task_state: active`. Both require the current owner; the shared CSRF, maintenance and bounded-read layers apply. The immutable human Graph journal includes the selected node in the original Graph payload before the task becomes ready. Reusing an operation with different bytes, project or worker conflicts, including concurrent admissions. Status/recovery reuse `/api/human/operations/{operation}` and `/recover`.

Initial native polls skip work assigned to another node. After a retained Started receipt exists, subsequent normal recovery/handoff can choose another eligible worker. All existing project authorization, source, capacity, harness, credential, grant and confinement checks remain authoritative. Starting is never reported merely because Graph accepted a task.

The tab retains the frozen launch intent per principal, authorization revision and node. Lost replies expose Check status, Recover launch or Retry same launch. Selecting another worker cannot reuse that intent. When the real Swarm projection reports its agent, Open agent terminal selects the existing live terminal pane. Clicking the tree's agent does the same. Terminal watch/control policy remains unchanged.
