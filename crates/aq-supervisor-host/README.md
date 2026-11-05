# Central supervisor host (F1a)

`aq-supervisor-host` runs a centrally identified session with the published
`aq-session-host` 0.1.6 guardian/tmux runtime. It has no worker enrollment,
assignment offers, Docker API client or remote command endpoint.

The Rust `Supervisor` API accepts an installed `PlanAuthority` and `HostAdapter`.
It joins every CentralPlan binding to HostPlan 1, persists launch callbacks in
PostgreSQL, and serializes physical operations using an actor advisory lock on a
dedicated connection. Start replay observes the recorded guardian; it never
replays physical launch. Every re-adoption checks current policy and a live
same-boot central permit. Renewals commit the next central sequence before
updating the guardian. Cancellation, expiry and failed observation stop and
preserve scratch. Only the shared runtime's stopped proof permits a successor;
missing proof persists StopUncertain. The SQL owner retains all generations and
constructs MailHistoryFence from the entire history.

## Installed executable

Commands are `run CONFIG`, `stop CONFIG` and `status CONFIG`. CONFIG is an
absolute, canonical, owner-only regular JSON file, at most 64 KiB, matching
`src/config.rs::Config` (unknown fields reject). It is trusted local admission,
not an unauthenticated plan submission interface. Install the exact approved
CentralPlan, HostPlan, resolved model, startup file and runtime paths. Changing
or removing the file revokes that process's admission. The host uses a database
already migrated by aq-server (F1b requires state format 48 or later). It never
migrates the database itself. A new plan does not bypass a retained generation's stop or mail
fence. F1b supplies application dispatch and original-mail authorization.

After Running, `run` verifies the exact staged turn's handoff and issues the
generation-bound supervisor reply credential. It installs the token as
`/host/<launch-id>/supervisor.credential` (root-owned 0600), with expiry no later
than the generation's original deadline. Re-adoption authenticates and reuses
the installed token; it does not rotate a live native session's credential.
An invalid installed token or unverified handoff refuses activation and retains
the launch for inspection.

The native wrapper waits for this installation before spawning Codex, then
stages a 0600 copy in that session's private native home, owned by UID 10001.
`AQ_SUPERVISOR_CREDENTIAL_FILE` names this copy, and
`AQ_SUPERVISOR_DISPATCH_ID` identifies the turn. Both are explicitly available
in the native and tool shell environments; neither contains credential bytes.
Supervisor tool networking is enabled so it can POST
`/v1/supervisor/conversation/replies` on the installed AQ machine listener with
the file's contents as the bearer token and the dispatch ID in the request.
The listener origin remains deployment/startup context, not a worker identity.
Only the central supervisor recipe receives this access. Keep `/host` and
`/native-homes` private to the supervisor host and absent from worker mounts.

Run the container with its own PID namespace. Do not use privileged mode, the
host PID namespace or a Docker socket. `/host` must be a persistent canonical
root-owned 0700 directory. `/workspace` belongs to UID/GID 10001; `/native-homes`
is a root-owned traversable directory (0711). Preserve `/host` across controller
restarts, and retain it for inspection after container/guardian loss. A container
restart that kills the guardian cannot re-adopt it or manufacture stopped proof.

`run` handles SIGTERM (the default `docker stop` signal) and SIGINT by calling
the verified stop path. Set the container stop grace period greater than the
10-second `wait_stopped` budget plus database lock/statement timeouts and
scheduling overhead; the default 10-second Docker grace period is insufficient.
Force-killing the container can still leave stop uncertain.

A crash after `StartCommitted` but before the host start-intent fact requires
operator inspection of `StopUncertain`; a crash after `Prepared` but before the
start commit is reconciled to `CancelledBeforeStart` and requires a successor
plan rather than retrying the same plan.

The root host reads `secret_file`, the protected-file placement primitive used
by `CredentialConfig.secret_file`. Mount it root-owned 0600, without links,
outside the workspace; it must contain at most 4096 bytes of one printable key
(with an optional final newline). It is read only by the guardian-owned scoped
transport, rechecked for identity/content on every request. Rotation revokes the
transport. Secret values never go in CONFIG, process arguments, environment,
startup text or the native sandbox. Do not call the server registry's material
loader to deliver this credential.

The native executable is a separately supplied, root-owned, non-writable,
executable regular file. Pin its SHA-256 in `native_digest`; the host executes the
verified descriptor. The required Codex recipe is **0.166.0**, from the published
shared mapper. A digest and version label do not qualify a native release: the
operator must install the qualified executable/environment for the pinned
adapter and environment references. Native subprocesses run as UID/GID 10001,
without supplementary groups, with `no_new_privs`, a clean environment and
startup on stdin. The native home contains only the scoped local transport
capability, never the upstream provider credential. The root transport disables
process dumps and stays within the guardian kill domain, so controller restart
does not rotate a still-live transport.

The image builds the host/helper from the vendored dependency closure and checks
its runtime sources against the hash-verified release archive. Source provenance:
aq-worker `4992a16d6c2f794d692803fbe25e69ea400a97db`, source archive SHA-256
`ab37cb192870f07290b4aff887b57b73783931ba5dfc248184da483c99036bdb`.
Only Cargo manifest workspace/path normalization differs from the release; each
original manifest is retained as `Cargo.toml.upstream`. No sibling checkout is
needed. The existing terminal-relay and session protocol source matches the
published closure.

## Limits

ResourcePolicy::CallerAdmission records admission; it does not claim cgroup
memory/CPU/process enforcement. Real Codex/provider qualification, aggregate
spend enforcement, hostile-kernel isolation, E1c production Compose, F2 enqueue,
H4/H6 waits and J4 final security
review are outside this increment. The physical acceptance tests use a native
shell fixture and real PostgreSQL, guardian and tmux. The container boundary
check uses a fixture CLI and a dummy provider key; it makes no provider request.
