# Controller-scoped resize of managed task and attempt terminals

A terminal pane is resized only by its **current controller**. The owner approves
resize by being that controller; there is no separate resize grant. A watcher, a
read-only viewer, a stale controller (an older generation, a released or replaced
lease, an expired lease) and a revoked connection never reach the worker. Two
browser paths exist and the dashboard picks one per terminal, as for typing
(`docs/specs/0009-agents-terminal-workspace.md`, "Queued attempt terminals").

| Path | Used for | Authority checked |
| --- | --- | --- |
| Finite relay `POST /api/v1/terminals/{terminal}/mutations` | A terminal whose finite policy has input on (`input_enabled`) | The scope `Right::Control` of the signed-in owner, the exact terminal identity, session revision, lease, controller generation and execution expiry |
| Live attempt socket `GET /api/attempt-terminals/{attempt}/terminal/live` | A running docker-local attempt (finite input off) | Same-origin, the signed-in owner, the connect `client_id` and `generation` of the current controller, and `read_only=false` |

The browser never sends a principal, a node, a boot or a connection id. The server
derives them.

## Finite relay

Existing grants and checks, unchanged: `Right::Control` (an owner scope with
control, not merely inspect); the request's `terminal`, `expected_session_revision`
and `expected_controller_generation` must equal the stored terminal; `lease_id`
must be the unexpired, fence-acknowledged lease of the caller; `expires_at_ms` may
not pass the grant or the access window. Mutating requests carry a unique
`operation_id`: an identical retry returns the original reply and queues nothing, a
changed body under the same id is a conflict. `sequence` must be greater than the
terminal's last resize sequence (a replay or older sequence is stale). The
published request is `ResizeRequest` in `aq-session-api`
(`protocol/session-api/v1/examples/remote-resize.json`).

Request (`Mutation::Resize`, adjacently tagged `kind`/`body`):

```http
POST /api/v1/terminals/5e0d8b9a-6c1e-4f43-9f7e-2b2b7c4c1d10/mutations
X-AQ-Request: 1
Content-Type: application/json

{"kind":"resize","body":{
  "terminal":{"session":{"kind":"remote","node_id":"11111111-1111-4111-8111-111111111111",
    "boot_id":"11111111-1111-4111-8111-111111111111",
    "attempt_id":"11111111-1111-4111-8111-111111111111",
    "assignment_generation":1,
    "session_id":"11111111-1111-4111-8111-111111111111"},
   "terminal_id":"5e0d8b9a-6c1e-4f43-9f7e-2b2b7c4c1d10","epoch":1},
  "operation_id":"0a3f1e52-9d0c-4d0a-8f1e-6b8f0c2f5a77",
  "lease_id":"3c1c2b7e-5d44-4c9e-9a53-0f6a5d1b9e01",
  "expected_controller_generation":3,
  "expected_session_revision":1,
  "expires_at_ms":1790000002000,
  "sequence":1,
  "columns":120,
  "rows":40}}
```

`lease_id` and `expected_controller_generation` come from the `control` mutation
reply (`{"kind":"control","body":{"operation_id":...,"status":{"lease":{...},...}}}`),
`expected_session_revision` from the terminal info, and the clock from the
`aq-terminal-server-time-ms` response header. `expires_at_ms` is at most 2000 ms
after the server time. Unknown fields are rejected.

Reply `200`: the resize is queued for the node, which applies it and acknowledges
by `sequence`; the browser reads the result from a later terminal info/output
response, not from this reply.

```json
{"kind":"resize","body":{"terminal":{...},"operation_id":"0a3f1e52-9d0c-4d0a-8f1e-6b8f0c2f5a77","sequence":1}}
```

Bounds: `columns` and `rows` each `1..=1000` (`u16`). Anything else is `400`
(invalid) and queues nothing. A request without control scope is `403`; a stale
terminal, session revision, generation, lease, sequence or operation reuse with
other bytes is `409`; a request over its expiry or grant is `409` (expired). The
`unsupported` outcome the finite relay returned for resize before this change no
longer occurs.

## Live attempt socket

Connect first, exactly as for typing (`POST
/api/attempt-terminals/{attempt}/terminal/connect` with `{"client_id":<uuid>}`
returns the `generation`), then open the socket with the `Origin` of the dashboard:

```
GET /api/attempt-terminals/{attempt}/terminal/live?client_id=<uuid>&generation=<n>
GET /api/attempt-terminals/{attempt}/terminal/live?client_id=<uuid>&generation=<n>&read_only=true
```

An attempt attach takes no `columns` or `rows` (the pane keeps its launch
geometry); the server's first text frame is `{"kind":"ready","server_time_ms":...,
"columns":120,"rows":40}` with the pane's geometry. Frames before `ready` are
dropped, as is input. After `ready` the current non-read-only controller may send
one text frame:

```json
{"kind":"resize","columns":100,"rows":30}
```

Bounds on this socket: `columns` `2..=500`, `rows` `1..=300`, both integers and
both present. Extra fields (such as a `connection_id` or `generation`) are ignored
and never forwarded. The server forwards to the worker, on the existing relay
stream, with the connection and generation it checked, and nothing else:

```json
{"kind":"resize","columns":100,"rows":30,"connection_id":"<uuid of this attach>","generation":3}
```

The worker answers nothing on this frame; the pane's new size shows in its output.
A resize does not count against the input byte budget. It is rejected, with the
socket closed and a `detach` sent to the worker, when the attach is `read_only`,
the frame is malformed or out of bounds, the controller changed (a newer
`connect` by another client, an expired control, a different generation), the
owner's session ended, or the node was revoked. A watcher that must resize
reconnects with a fresh `connect` and no `read_only`.

Manual agents keep their own socket behavior (the viewer's `columns` and `rows` on
attach, `{"kind":"resize",...}` after it, same bounds), and worker host shells are
unchanged.

## Tests

- `crates/aq-terminal-relay/tests/fake_peers.rs`:
  `resize_reaches_the_worker_only_from_the_current_controller_within_bounds`
- `crates/aq-manual-sessions/src/stream.rs` unit tests (frame rules and bounds)
- `crates/aq-server/tests/it_dashboard/terminal_output_poll.rs`:
  `resize_is_accepted_only_from_the_current_controller_within_bounds` (needs the
  test PostgreSQL)
- `crates/aq-server/tests/it_server/manual_agents.rs`
  `queued_attempt_terminal_uses_the_live_stream_of_its_own_running_node` (forwarded
  resize, read-only refusal, stale controller refusal; needs the test PostgreSQL)
