# B2 runner receipt transport, revision 3

This is the server-owned mapping for B6a2's injected `RunnerExchange`. It is
requested by `msg-f8f7d6189f034835a5b9973ab1c0379c` / task comment
`comment-4be01fc316134eedb25c4df0b29fffd4`. The comparison source is aq-worker
`210e51861fa8008cc8f2ac034c8e1cf477e7d3f0`, `docs/implementation/B6a2.md`.
Pin the server commit containing this document and the fixture manifest beside
the evidence. This publication does not install a worker codec or qualify its
production CLI end to end.

The sandbox owner dependency is the unmodified aq-admission 0.3.2 archive at
aq-worker `6c06d98f74520871c99f5fb9a0cf893c0e122fed`; its SHA-256 is
`89cc29b239d849ac968d3a57d86420961062009dc966928d47fd91411132e1d8`.
This release is pinned from the published branch archive while integration delivery
is pending. The 120 member hashes are pinned in `vendor/admission-contract.json`. The frozen
phase/result bodies remain admission 0.1.11; the source transport bodies remain
aq-worker-protocol 0.2.0. New headers supplement these original bodies.

## Authentication and correlation

Ordinary sandbox runner requests use the permit Bearer credential and
`aq-runner-channel` containing bare JSON `RunnerChannelIdentity`. Node registration and re-adoption
remain node-only. The server checks attempt, generation, channel, active boot,
current permit, cancellation and deadlines under effect-owner locks. Source
transfer IDs are looked up to recover their owned attempt before authorization;
an unrelated attempt is refused even for the same node.

Successful source status/finalize and result responses have `Cache-Control:
no-store`. When an authenticated runner receives a confirmed receipt, the
`aq-runner-receipts` header contains bare JSON of this server-owned envelope:

```json
{"schema_version":1,"source":null,"result":null,"finished":null}
```

Each non-null source/result is B6a2's `CentralReceipt`:
`{receipt_id, attempt:{launch_id,attempt_id,manifest_digest}, artifact_digest}`.
Each non-null finished is `{receipt_id,attempt}`. UUIDs are strings; digests are
lowercase SHA-256 hex strings. The strict Rust definitions are
`delegated_permits::receipts::{Envelope,AttemptKey,CentralReceipt,FinishedReceipt}`.
Require exactly one header, at most 8,192 bytes, schema version 1, nonnil IDs and
the exact expected attempt key; reject unknown fields. Do not infer receipts
from a successful status, a supplied operation ID, or a local file.

`launch_id` comes from the authenticated native Started history, or the result's
verified matching stopped evidence. The runner's frozen local launch ID must
equal that identity. `manifest_digest` is the original launch-manifest digest,
not the source-transfer manifest digest. Native report bodies keep their original
boot after Option A re-adoption; the credential/channel use the adopted boot.

## Renewal

`POST /v1/worker/profiled/renew` retains the published `SandboxDocument` request
and response, and requires `aq-delivery-nonce`. Successful delegated responses
also carry:

- `aq-permit-context`: bare owner `PermitContext` for the operation's original
  request, before its lease mutation. Exact replay keeps that request context.
- `aq-server-now-ms`: positive decimal server milliseconds. A renewed response
  uses its delivery clock time; an awaiting response has no delivery clock and
  grants no additional time.

These fill B6a2's `RenewalDelivery { response, context, server_now_ms }` without
changing the owner body. Validate response/request/nonce/revision with the pinned
owner validator and the original transport send/receive boot observations.
Awaiting, unavailable, malformed and replayed replies never restart a TTL.
HTTP 401/403/409 represent refusal; transport failures, 429 and 5xx represent
unavailability. Neither grants authority. Malformed success is a local refusal.

## Retention and result completion

The runner first proves local stop and preserves its workspace. It must construct
the existing native phase stopped report from that proof and the exact Started
history; raw host-proof bytes alone are not the stopped wire request.

1. Send the nominal phase `StoppedRequest` to
   `POST /v1/worker/profiled-v2/stopped`, with
   `aq-phase-terminal-api-version: 1` and `Content-Type: application/json`.
   Validate the nominal `StoppedReceipt` against the exact request. Both frozen
   phase-v2 and refresh-v1 retain their own types. Persist operation intent before
   sending, and persist the receipt before constructing the result report.
2. Export an actual stopped-workspace checkpoint using the existing worker Git
   owner. `POST /v1/execution/sources/begin` accepts the existing `BeginRequest`
   containing node identity, operation ID and `TransferManifest`. It must bind
   the original attempt/spec/source repository/base and exported checkpoint.
   PUT exact raw chunks to `/v1/execution/sources/{transfer}/{manifest|bundle}`
   with query `operation_id`, `offset`, `digest` (SHA-256 of that chunk).
   Chunks are at most 256 KiB. Request/transfer IDs and bytes remain fixed on retry.
3. `POST /v1/execution/sources/finalize`, and status at
   `POST /v1/execution/sources/status`, accept `TransferStatusRequest`:
   `{identity,operation_id,transfer_id}`. The response is the original
   `TransferStatus`. Only a confirmed non-null central receipt yields the sibling
   `source` projection. Pending/import-unknown responses are not receipts.
   The receipt ID is the central confirmation's receipt ID; `artifact_digest`
   is the canonical Git manifest digest from the verified import, **not** the raw
   JSON transport digest or bundle digest. The original manifest records those
   independent transport hashes. Central Git verifies/imports the actual bytes
   before the server commits this receipt. A historical receipt alone is not a
   claim that retention remains active forever.
4. Build the frozen `ResultReport`, including the confirmed source receipt ID,
   exact original binding and verified stopped evidence. Send it to
   `POST /v1/worker/profiled/results`. Validate the original
   `result_record::Receipt` against that exact report. The sibling `result`
   projection uses its report ID and canonical report digest. It proves retention
   of the report bytes, not arbitrary files or an unuploaded transcript. Preserve
   required workspace/output bytes through the actual source checkpoint or an
   independently supported retention owner.
5. The same successful result response includes `finished`, with the same report
   ID and attempt key. B6a2's `retain` can persist both central receipts and cache
   the original report/finished reply. Its `finished` call can return that checked
   retained reply, or retry the exact same result request and validate the same
   reply. No second mutation or new operation ID is needed. `FinishedReceipt`
   means the runner's result report is recorded. It does not complete the Graph
   task, accept its result, release claims, or authorize deleting unretained data.

All delegated retention/report operations use execution-scoped `ReportResult`
and remain bounded by the current grant and permit expiry. The node's separate
preserve/stop recovery path remains available when runner authority has expired.
After B3 cancellation revokes the delegated permit, B6a2/B5b must use that native
node credential path for the stopped report and source/result upload. The
node/controller sends these recovery requests itself, preserving the original
native bindings and exact retry intent. It must never pass the node credential
into the sandbox, revive the revoked permit or invent a new TTL.

## Differences and current limits

- B6a2's `RunnerRoute::Artifact` points at `/v1/worker/profiled/artifact`, which
  downloads retained material. It is not an upload. Add the explicit source
  begin/chunk/finalize/status routes above to its closed route set.
- B6a2's stopped route points at the legacy `/v1/worker/profiled/stopped`.
  Phase sandbox execution uses `/v1/worker/profiled-v2/stopped` and its nominal
  version/header contract above.
- Use the server envelope's original Started launch identity. Do not substitute
  a fresh local launch UUID. Preserve original native boot bindings in report
  bodies after re-adoption.
- The 0.3.2 owner validator accepts exact stopped retries after Option A
  re-adoption. The server independently authenticates the current credential,
  channel and incarnation, derives trusted PermitContext under owner locks and
  loads the original report from retained server state. The historical report
  keeps its original boot. The retry returns the retained receipt without a
  second stop effect or lease extension. Caller-supplied context or an alleged
  original report cannot establish this authority. Rewritten report boots or
  operation IDs conflict; stale credentials, forged channel boots and expired
  authority remain refused. Ended renewal and other execution operations remain
  forbidden. A node rebooting after native stop but before receiving the stopped
  receipt or submitting its result can re-adopt the unexpired, uncancelled ended
  permit for these reporting operations only. Readoption replay preserves its
  deadlines and revision. This replaces the 0.3.1 refusal documented in checkpoint
  296c2d91.
- Full process restart qualification and a production B6a2 codec remain separate
  acceptance work. Store reconstruction and retained-receipt replay establish
  only the tested server persistence boundary.

## Pinned consumer fixtures

`B2-evidence/runner-receipts/sha256.json` pins eight JSON fixtures captured from
actual router responses under authenticated test credentials. These are disposable
test identities and credentials, never deployment credentials. The fixtures contain:

- `renewal`: exact request/response, returned context and server clock.
- `readoption`: committed adoption and its preceding awaiting reply.
- `readoption-stopped-replay-accepted`: original committed stop, adopted permit,
  trusted ended context and the exact retry's HTTP 200 with an identical retained
  receipt after Store reconstruction. The owner validator accepts original-boot
  history while binding the permit to the current incarnation.
- `source-PhaseV2` / `source-PhaseRefreshV1`: retained source response and envelope.
- `result-PhaseV2` / `result-PhaseRefreshV1`: native report/receipt and envelope,
  correlated with that profile's source fixture. Both use actual Git export,
  upload and central import; host-stop evidence is synthetic test setup.
- `result-finished`: same-boot runtime report and its stopped history; the runtime
  test also reconstructs Store, retries the exact report, verifies identical
  response bytes and one retained report, then checks expiry refusal.

`published_runner_receipts_match_native_owners` checks the fixture inventory and
hashes, invokes native validators and compares the source/result/finished attempt
keys and receipt IDs. The runtime tests separately exercise HTTP authentication,
re-adoption, exact replay, changed requests and expired authority. These checks do
not qualify a worker process, physical stop implementation or full daemon restart.
