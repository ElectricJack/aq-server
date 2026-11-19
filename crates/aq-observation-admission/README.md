# aq-observation-admission

Portable observation-store prepare/activate/boot ownership contract and pure
transition reducer. It depends on the exact published CON03 protocol and small
serialization crates. No database, filesystem, HTTP, provider client or executor.
CON03 packet and ACK bytes remain unchanged.

Requests, replies and statuses are at most 16 KiB. Version1 requires exact IDs,
owner revision checks and at most64 boot transitions. Hashes use the same compact
UTF-8, recursively key-sorted typed JSON encoding as CON03. The durable adapter
owns node authentication/revocation, current incarnation, permanent receipt
lookup, no boot reuse, atomic owner+receipt persistence and retention quotas.

An immutable receipt is paired with a fresh current owner snapshot. Replaying a
prepare receipt after activation cannot permit store creation, including when
central accepted-through is stillzero. A status response is a separate type and
cannot replace preparation. `Reply::may_create` is only one condition: the worker
must freshly authenticate the response and enforce its durable local create
intent, store identity and uncertain-create/missing-store fences. Cached receipts
or deserialized proof wrappers do not authorize rebuilding lost data.

Prepared stores cannot enroll sources, collect records, or emit facts. Activation
must follow durable store creation; lost activation replies resume only an
existing exact store. A boot transition preserves the immutable original marker
and accepts bounded pending old-boot records without acknowledging them. Its
source-fencing transaction must finish before new live collection. Missing active
stores require an explicit future recovery protocol, not a new prepare call.

`apply` consumes central owner inputs; it is not a verifier for worker-supplied
owner state or receipts. It returns an immutable receipt and current owner to be
committed atomically. `Access::new_boot_previously_owned` is an indexed check of
permanent initial/boot receipts; exact old receipt replays do not roll state back.
No transport result grants execution, source registration, fresh-zero cumulative
baselines, native parser qualification, or complete accounting.

`cargo run -p aq-observation-admission --example export` regenerates schemas and
synthetic examples. Rust validators enforce cross-field constraints in addition
to JSON Schema shapes. The server adapter and local store handshake require
separate qualification; publishing these types alone does not enable them.
