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
