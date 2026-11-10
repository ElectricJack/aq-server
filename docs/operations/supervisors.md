# Central supervisors

Global and project supervisors use the retained mailbox and dispatch protocol.
Each project has a distinct immutable conversation, actor, controller, container,
native home and workspace. These are central services, not enrolled workers.
Starting one does not consume a worker slot. Implementation is delegated to AQ
tasks and remains visible in Work and Agents.

Open Supervisor, choose a scope, select a saved Codex intelligence profile, and
Start. The controller reports Ready between turns and Working during a physical
generation. Confirmed task creations appear as task links beneath its reply.
Proposals use draft tasks; active work requires an execution request. Stop
revokes tools immediately and waits for physical stopped-writer evidence.
History and task operations remain retained. Profile changes affect subsequent
turns. Failed or uncertain requests are never automatically replayed: inspect
the retained dispatch and actual task state first.

The installed central adapter supports Codex CLI 0.166.0 and OpenAI profiles with
low, medium or high effort, using subscription login. Other providers remain
available for manual worker sessions. Turns respect the existing one-hour central
authority limit and renew the physical permit. Prior history is frozen with new
input (32 messages / 64 KiB); it is context, not re-reserved work. Workers can
send completion and blocker messages to their supervisor conversation.

## Local fleet manager

`scripts/supervisor-service-manager.py PRIVATE_CONFIG` is a trusted operator
service with Docker access. It creates only scope-labelled
`aq-ms1-supervisor-*` containers from a digest-pinned host image. Each has a
read-only root filesystem, private writable workspace/home/tmp, private PID namespace, 0.5 CPU, 2 GiB memory, 512 processes, a distinct durable
state volume, and no Docker socket. The manager never removes, replaces or
blindly restarts retained containers. Upgrades and uncertain generations need
operator inspection. Stopping the manager leaves controllers alive; dashboard
Stop requests a confirmed native stop.

The mode-0600 manager JSON supplies `image` (Docker sha256 image ID), `prefix`,
`network`, `postgres_container`, `subscription_file`, and `service`. The latter
is ServiceConfig without conversation_id: private PostgreSQL URL, authority_id,
root, workspace, homes, helper, tmux, tmux_version, native, native_digest,
image_digest, secret_file and machine server_origin. The manager supplies the
scope and copies protected configuration and ChatGPT authentication through
stdin before startup. Host authentication is never rewritten; validated
refreshes stay in the private container cache. Native processes use UID 10001;
lifecycle, admission and database material remain root-only.

The image and executable digests are in the immutable turn plan. Install the complete pinned Codex package: codex, adjacent codex-code-mode-host, codex-path/rg and codex-resources/bwrap. Verify all four against the matching aq-worker native-runtime pins. The CLI alone can answer text but cannot execute tools. This local
subscription adapter does not claim a new native mapper/resource qualification
or hostile-network containment. The development stack network is trusted.

Tools expose current scoped reads, task creation, task editing, priority and
dependencies. Project supervisors can only discover, read and mutate their own
project. Tools check current owner identity, enabled control, generation, scopes
and central head under the central fence. Discover schemas with
`aq-supervisor-host tool commands [NAME]`; invoke NAME with one CommandRequest
JSON on stdin. Owner configuration commands remain unavailable. Exact mutation
identities and receipts are retained across uncertain responses.
