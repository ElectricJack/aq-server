# aq-phase-terminal-protocol 0.1.0

Pure finite envelopes over frozen `aq-admission =0.1.9`. This package changes no
admission module, archive, host observation or existing protocol feature enum.
The profile `aq.phase-terminal.v1` requires the exact published contract artifact
and both ordered features `started_stopped_closure_v1` and
`original_lease_no_renewal_v1`. Unsupported negotiation refuses without fallback.

`StartedRequest`, `StoppedRequest` and `ClosureRequest` wrap the original bodies
and an `Origin` containing actual execution/accepted receipt digests. Their
borrowed, non-Serde `Context` contains the actual retained phase history,
runtime/qualification and **server Prepared checkpoint**. This checkpoint equals
the retained Prepare request's journal revision/digest; it is not the worker's
later local commit. Closure checks the phase associations directly, without a
legacy `GrantedContext`. Stopped additionally checks the matching original
Started guardian, pane, kernel identity and monotonic observation/journal facts.

Call `decode_*_request` at the wire boundary, then the appropriate `validate_for`
with actual owner-loaded context. Shape validation and schema conformance do not
authenticate a worker, prove kernel/fdatasync facts or current execution authority,
prove the absence of Started, or compare newer acknowledged renewal counters.
Those remain the owning server's duties. The first profile has no renewal.

Acknowledgments bind the **complete outer request digest** as well as the frozen
inner body's request digest. `*Receipt::validate_for` checks a retained request;
these historical facts have no delivery clock, live permission, resource release,
task success, or resumed execution meaning. `HistoricalReadback` contains exactly
one receipt and checks its matching `TerminalRequest` variant. Current inspection
authorization is independent of historical structural validation.

A worker that lost its first execution reply can issue `HistoryRequest` using its
known accepted-receipt digest, Prepare operation and exact binding. A
`HistoryReceipt` returns only finite historical identity pins and a projection
sample time, not an ExecutionReceipt/ExecutionDelivery or opaque live worker
object. Its nullable identity is required on the wire. Null is snapshot absence,
**never NeverGranted or proof of no issuance**; a later read of the same lookup may
return a known identity. Known identity includes the original Prepare digest,
runtime, qualification, Prepared checkpoint, versions and hard deadline. History
alone cannot create a live permit or authorize a host hook.

Every complete request, acknowledgment and readback is bounded to 65,536 bytes,
including its wrapper. Wire decoders also bound whitespace. Existing tighter
inner bounds remain in force. Strict structs reject extra/duplicate fields and
unknown enum variants. Typed integers reject floats. Canonical digests recursively
sort JSON objects independently of `serde_json/preserve_order`, encode compact
JSON and hash `[domain,document]`. Separate domains distinguish each request,
acknowledgment, readback and contract manifest. The artifact digest pins the
schema digest, package/dependency versions, byte bound and exact feature list.
JSON Schema cannot establish relational associations or byte limits; Rust
validators remain mandatory.

`examples/export_phase_terminal.rs` deterministically exports `schema`, `manifest`,
`artifact`, `fixtures` (default), and `invalid`. Drift tests compare retained
artifacts. Fixture history/host observations are sanitized synthetic correlations,
not authenticated daemon/worker/kernel evidence. This package qualifies no route,
SQL migration, physical producer, provider, renderer, snapshot barrier or runtime
activation. The paired owner/proof qualification is separate.
