# aq-wait-engine

Pure, bounded verified-wait transition plans using frozen CON03 contracts. The
crate has no database, async runtime, graph client, driver, public routes or
execution/physical-stop authority. It is workspace-only (`publish=false`).

The caller supplies an authenticated opaque Binding/authority fence and resolved
policy, serializes the known-component cycle check with registration, and applies
each `TransitionPlan` using its expected revision in one durable transaction.
It must persist intent/work before IO, admit physical source work with finite
nonwaiting bounds, verify actual source-owner receipts, and persist terminal
outcome plus resume intent together. Nothing here installs a scheduling hold or
claims a source subscription was applied merely because an action was planned.

`SourceOwnerPort` is an internal trusted adapter boundary. Source replies are not
deserializable agent input. A `CommittedReplay` declaration/digest is not a
cryptographic proof: the adapter must qualify its real owner's committed replay
and snapshot protocol. Arbitrary event payloads and allocated sequence numbers
cannot satisfy a wait. Source events only coalesce dirty work; installation must
be followed by unconditional reobservation. Gaps/unavailable coverage stay pending
with backoff; they never become current-only success or source failure.

Use `Snapshot::decode` for bounded restart data. Snapshot validation checks shape,
identity/digest linkage and reducer invariants; the outside storage owner still
authenticates retained bytes and current authority. No snapshot is itself a grant,
enforced-hold receipt, stopped proof, retained checkpoint or running successor.

The first slice exposes no public `WaitRegistered` response. Real AQ08 hold
enforcement, Graph committed source protocol, WRK16 parking and AQ26 wake/ack
remain separate. See `docs/implementation/AQ-25-foundation-proposal.md` in the
workspace root for the source audit and integration boundaries.
