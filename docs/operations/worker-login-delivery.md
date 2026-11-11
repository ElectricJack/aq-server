# Login delivery to connected workers

The owner can select a worker in Agents → Swarm and push AQ's saved Claude and
Codex subscription logins. New hosted installations include a small user timer
that receives these requests. Existing installations need the latest hosted
installer once; enrollment and configured capacity are retained.

Owner routes are `GET /api/worker-logins` (status only) and
`POST /api/worker-logins/{node}` with an operation identity and a bounded provider
list. The worker-only listener exposes `POST /v1/worker/logins`. Its authenticated
receiver polls and acknowledges one generation. A request remains pending while
the worker is disconnected or its machine has active agents. A live pending delivery
briefly reserves the machine against new admissions so a restart cannot race
with a new agent. That reservation expires if the receiver stops reporting. Failed installation
is visible and can be retried with another push. An uncertain browser request
reuses its original operation identity and cannot replace a later request.

Only enabled, provisionable subscription logins permitted for this enrolled node
and the docker-local backend are delivered. Delegated agent credentials, revoked
nodes, retired nodes, API keys and credentials outside placement policy are
refused. The configured file fingerprint and newest subscription lineage are
checked again against the current registry and node before private delivery.
Private responses use `Cache-Control: no-store`; SQL stores provider names,
request identities, receiver times and receipts, never login material.

The worker receiver uses the enrollment's HTTPS origin, refuses redirects, writes
private files atomically, enables manual provider configuration, and restarts only
when new configuration or provider support requires it. Updating a login at an
existing path makes it available to subsequent agents. Personal login files are
kept. This is owner-requested delivery, not an automatic subscription refresh or
write-back mechanism for interactive agents.

Implementation decision (my call): ship the receiver as a Python user timer in
the hosted installation so existing pinned worker binaries can gain this feature
without replacing their runtime or consuming another enrollment code. The runtime
already reads the resulting private paths. A future native receiver can implement
the same delivery/receipt contract.

## Remote checks and worker control updates

Owners can queue a fixed sandbox startup check from the selected worker's pane.
`POST /api/worker-diagnostics/{node}` accepts an operation identity and one of
`sandbox_startup_v1` or `receiver_update_v1`. The latter requires the checksum of
a hosted installer. `GET /api/worker-diagnostics` returns the latest job and
receiver observation for each worker. These routes require the owner role.

The enrolled receiver polls `/v1/worker/diagnostics`, executes its built-in check,
and returns a bounded typed receipt. A missing/offline receiver leaves the job
queued. Results include rootless/cgroup settings, image presence, recent-session
metadata, and the exact create/start/tmux failure of an isolated probe using the
agent's Docker API settings. Provider files, node credentials, arbitrary host
commands and existing agent logs are excluded. The probe's uniquely named
container and volume are cleaned up. Receipts survive a lost acknowledgement so
retry does not rerun a completed check.

An installation predating this protocol needs the hosted installer once. Later
receiver updates are owner-requested and remote: download the checksum-pinned
installer only from the saved HTTPS origin, refuse redirects, extract and compile
the embedded receiver, then replace only its private Python file atomically.
The installer shell is not executed. Runtime binaries, enrollment and running
agents are retained. The next timer invocation loads the new receiver.

Migration 112 stores diagnostic jobs and receiver observations. Report shapes
and sizes are validated before storage; a worker can report only its own job.
