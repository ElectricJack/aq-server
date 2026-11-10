# Manual interactive agents

Owner direction (2026-11-10): manual launch creates a persistent interactive
agent, with a project but no assignment. Startup belongs in the graph. Worker
slot controls are scoped to the selected project. Terminal input connects
automatically; there is no user-selected watch/edit mode.

The manual lifecycle is separate from finite queue tasks, grants and result
reports. `aq-manual-sessions` owns retained session intent and project slot
limits, owner start/stop, enrolled-worker claims/progress, and bounded disposable
terminal state. Manual sessions do not manufacture Graph tasks or qualification
artifacts. An installed manual-capability report is not a native qualification.
The original qualified task runner and its historical protocols remain in place.

Owner routes are under `/api/manual-agents`: list, worker/project start and slot
updates, stop, terminal connect/snapshot/input and operation receipt reads. They
use the ordinary owner/CSRF guards. Worker reports and claims use
`POST /v1/worker/manual-agents` under the original enrolled node credential;
delegated attempt credentials are refused. Worker mutations are bound to the
original node, and revocation is checked while its row is locked. A stopped
project/worker stops rolling authorization; drain preserves running sessions.

The worker's optional `[manual]` service configuration names an absolute
operator-installed Podman client, hard slot bound (1–8), and optional
`source_slugs` mapping installed repository IDs to explicit GitHub slugs. No
URL, executable, credential, mount or shell command is taken from a launch
request. Source is fetched inside the existing fixed-policy sandbox; immutable
sources are exact commits and named refs use the configured remote. Local-only
source branches are not silently published or substituted. The existing
subscription-token file is read privately and delivered as private material;
only Claude receives the token environment. Worker credentials never enter the
sandbox. No API key is configured.

A tmux session lives inside the sandbox and runs Claude without print mode or an
initial assignment. Source download retries while the worker remains healthy.
Startup stages reflect actual preparation. The worker renews a 45-second
liveness watchdog; closing the dashboard or completing a turn does not stop it.
Stop confirms the sandbox stop and retains its workspace/container volume.
There is no automatic result publication or PR creation for manual sessions.
Native task admission on a worker waits while that worker has manual sessions;
other worker nodes remain independent. Reducing a project's slots queues new
sessions and preserves existing ones.

Terminal output is bounded, disposable replay in memory, not a durable transcript.
Input is ordered and identified by a unique operation, sent once, and acknowledged
only after worker application. An unknown delivery pauses typing; buffered bytes
are never replayed on reconnect. Existing native terminal controller fences and
authorization remain enforced after removing the UI mode selector. Manual tmux
uses its separate owner/controller generation and original session identity.

Validation and actual live session IDs are recorded in the supervision log and
operator QA directory. The backend integration test exercises real SQL and
production owner/node routes without inference; it does not qualify a model.
Private rollback backups precede promotion. Migration 107 adds retained manual
state: an older server that supports only format 106 cannot read that database.
Keep a compatible server when disabling the feature; do not restore a database
over newer owner work. The owner's separate local worker and Supabase are not
part of the deployment.

## Interactive transport (2026-11-10 performance correction)

Normal dashboard input/output now uses bounded raw WebSockets, through the owner
server and the original enrolled node, to a real disposable tmux PTY. Unlike the
initial capture-pane/HTTP receipt adapter, this preserves cursor, alternate-screen
and bracketed-paste state and does not serialize keystrokes behind worker polling.
The existing HTTP command/receipt routes remain compatibility endpoints.

`aq-worker terminal-relay --service-config <installed profile>` runs the independent
transport companion. It reads only installed enrollment and owned manual-session
state, opens one persistent Podman exec PTY for a viewer, and propagates actual
terminal dimensions. Closing/restarting the companion disposes attach clients;
it never stops or relaunches agent processes. The owner/node/project and original
controller generation are rechecked, connection identities fence late frames,
and bounded queues disconnect slow peers. Raw input is ephemeral and never
replayed on reconnect. Owner browser Origin is checked before WebSocket upgrade;
node credentials stay on the host and out of URLs/PTYs. There is no durable
transcript or durable per-key delivery claim for this SSH-style byte stream.
