# Central supervisors

2026-11-13: aligned with the rolling-records principle
([architecture 0006](../architecture/0006-records-are-rolling.md));
implementation pending.

Global and project supervisors use the retained mailbox and dispatch protocol.
Each project has a distinct immutable conversation, actor, controller, container,
native home and workspace. These are central services, not enrolled workers.
Starting one does not consume a worker slot. Implementation is delegated to AQ
tasks and remains visible in Work and Agents.

Open Supervisor, choose a scope, select a saved supervisor intelligence profile, and
Start. The controller reports Ready between turns and Working during a physical
generation. Confirmed task creations appear as task links beneath its reply.
Proposals use draft tasks; active work requires an execution request. Stop
revokes tools immediately and waits for physical stopped-writer evidence.
A conversation keeps its most recent 4,096 messages, and older ones are deleted
as new ones arrive; task operations keep their receipts for their replay window
(architecture 0006; pending, today a full conversation refuses new messages).
Profile changes affect subsequent turns. Inspect the retained dispatch and
actual task state before retrying a command whose outcome is uncertain.

When the guardian proves a turn never started, the controller retries its
retained messages automatically. Set `automatic_turn_retries` in the manager's
`service` JSON to change the one-retry default. Zero disables automatic turn
retries. The allowance follows the individual messages across batches and
controller restarts. A confirmed reply finishes the turn. If the turn may
have made a task change, the controller reports its dispatch ID and keeps
the messages pending for inspection, including across a stop and start.
An operator can stop and start to retry a verified nonstart after its
automatic allowance is exhausted.

The central fleet supports subscription-backed OpenAI profiles through Codex CLI
0.166.0 and Anthropic profiles through Claude Code 2.1.318 when Claude is installed
and configured. Each turn uses the harness selected by its saved profile.
Turns respect the existing one-hour central authority limit and renew the
physical permit. Prior history is frozen with new
input (32 messages / 64 KiB); it is context, not re-reserved work. Workers can
send completion and blocker messages to their supervisor conversation.

## Local fleet manager

`scripts/supervisor-service-manager.py PRIVATE_CONFIG` is a trusted operator
service with Docker access. It creates only scope-labelled
`aq-ms1-supervisor-*` containers from a digest-pinned host image. Each has a
read-only root filesystem, private writable workspace/home/tmp, private PID namespace, 0.5 CPU, 2 GiB memory, 512 processes, a distinct durable
state volume, and no Docker socket. Routine reconciliation retains existing
containers. The explicit upgrade command replaces a controller after a confirmed
stop. An uncertain generation requires operator inspection. Stopping the manager leaves controllers alive; dashboard
Stop requests a confirmed native stop.

The mode-0600 manager JSON supplies `image` (Docker sha256 image ID), `prefix`,
`network`, `postgres_container`, and `service`. The latter is ServiceConfig
without `conversation_id`: private PostgreSQL URL, `authority_id`, `root`,
`workspace`, `homes`, `helper`, `tmux`, `tmux_version` and machine
`server_origin`. For Codex, add `native`, `native_digest` and `secret_file`
(`/login/auth.json`) inside `service`, plus `subscription_file` at the top
level. The
manager supplies the scope and copies protected configuration through stdin
before startup. Every Codex-capable supervisor mounts a shared login volume
(`<prefix>login`) at `/login`, seeded once from `subscription_file`. A refresh
rotates the ChatGPT refresh token, so separate copies lock each other out; with
one file, validated refreshes are copied back under the host's lock on `/login`
and every turn starts from the newest. Host authentication is never rewritten.
Native processes use UID 10001. Lifecycle, admission and database material
remain root-only.

The image and executable digests are in the immutable turn plan. Install the complete pinned Codex package: codex, adjacent codex-code-mode-host, codex-path/rg and codex-resources/bwrap. Verify all four against the matching aq-worker native-runtime pins. The CLI alone can answer text but cannot execute tools. This local
subscription adapter does not claim a new native mapper/resource qualification
or hostile-network containment. The development stack network is trusted.

### Install Claude in the fleet

Build the supervisor host with the same command registry features as the server.
For a mixed fleet, install the qualified Codex closure in the base image at
the paths already used by the fleet. Use the SHA-256 of the trusted Claude Code 2.1.318 Linux native
executable for the deployment architecture. Create a build context containing
only that executable, named `claude`. Supply the digest-pinned supervisor base
image and the Claude executable digest to the overlay.

```sh
docker build --file Dockerfile.supervisor-fleet \
  --build-arg SUPERVISOR_BASE_IMAGE="$SUPERVISOR_BASE_IMAGE" \
  --build-arg CLAUDE_NATIVE_SHA256="$CLAUDE_NATIVE_SHA256" \
  --tag "$FLEET_IMAGE_TAG" "$CLAUDE_BUILD_CONTEXT"
docker image inspect --format '{{.Id}}' "$FLEET_IMAGE_TAG"
```

Set the manager's `image` to that Docker image ID. The manager writes the image
ID into the controller config for new containers. Set `claude_subscription_dir`
in the manager JSON to the canonical absolute path of a root-owned mode-0700
directory on the Docker host, inaccessible to untrusted host users. Put the
owner's single-line `claude setup-token` value in a root-owned mode-0600 file
named `token` under that directory. Run the manager with access to this file.
Set these fields within the manager's `service` object.

```json
{
  "claude": {
    "native": "/opt/claude/claude",
    "native_digest": "<verified lowercase SHA-256 of claude>",
    "secret_file": "/claude-login/token"
  }
}
```

The fleet manager mounts the directory read-only. The controller reads the
token for Claude turns and delivers it through the child environment. For
rotation, atomically replace `token` inside the same directory, retaining
ownership and mode. Upcoming turns read the new token. The shared mutable Codex
login remains at `/login/auth.json` and its existing refresh path still applies.
A Codex-only installation can omit both Claude fields. A Claude-only installation
omits the three Codex fields and `subscription_file`.

To verify the CLI and tool broker before rollout, run the pinned native binary
against the loopback qualification fixture. This uses synthetic credentials and
checks discovery, a task creation receipt and the final reply.

```sh
python3 scripts/claude-supervisor-cli-smoke.py "$CLAUDE_BINARY" --host-binary "$SUPERVISOR_BINARY"
```

### Upgrade an existing conversation

Keep the scope disabled during the upgrade. In the dashboard, stop the
supervisor and wait for its confirmed Stopped report. If the report says
Uncertain, resolve the retained generation before upgrading. Update the mode-0600
manager config and restart its reconciliation service with the new config.
Run the upgrade for the conversation's UUID on the fleet host.

```sh
scripts/supervisor-service-manager.py "$PRIVATE_CONFIG" upgrade "$CONVERSATION_UUID"
```

The upgrade waits for a clean controller exit and parks the old container under
a name ending in `-upgrade-backup`. It reuses the named state and workspace
volumes and the shared Codex login. The replacement starts with the scope
disabled and must publish its own Stopped report before the manager removes the
parked container. Once the command succeeds, select the saved profile and
start the scope. Repeat for the other conversations individually.

If the upgrade is interrupted, leave the scope disabled and rerun the command
with the same config. A failed replacement that already exited requires operator
inspection. Check the scope label and the replacement's error in the retained
state directory, then remove only the exited replacement container with plain
`docker rm` and retry. Preserve the parked container and the named volumes
throughout recovery.

### Claude repository access

With both `supervisor_full_access` and `delivery_playbook` enabled, Claude can
use `repo_sync` to fetch the latest trees of `main` and AQ task branches for an
authorized project. Run it again to refresh the trees. In the same turn,
`repo_read` lists a directory or reads one tracked UTF-8 file. Use an empty path
to list the root, and choose `main` (the default) or `aq/<task-id>` as the ref.

A Git subprocess has up to 60 seconds. The broker limits file reads and raw
directory listings to 128 KiB, individual Git pack files to 128 MiB, and a
turn's repository storage to 256 MiB. Each broker can sync up to 32 projects.
The host removes these shallow repositories from the turn's private home once
the turn is confirmed stopped.

Tools expose current scoped reads, task creation, task editing, priority and
dependencies. Project supervisors can only discover, read and mutate their own
project. Tools check current owner identity, enabled control, generation, scopes
and central head under the central fence. Discover schemas with
`aq-supervisor-host tool commands [NAME]`; invoke NAME with one CommandRequest
JSON on stdin. Files attached to an owner message are read with
`aq-supervisor-host tool attachment-text ID` (the ID from the turn's
`[attachment …]` line). It returns the extracted text, or `text: null` with an
explicit state and message. Only files of messages dispatched to that
supervisor can be read (see `protocol/human-supervisor-conversation/v1`). Owner
configuration commands remain unavailable. Exact mutation
identities and receipts are retained across uncertain responses.
