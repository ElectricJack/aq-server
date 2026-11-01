# Phase terminal Stage A prerequisite

This checkpoint contains an unregistered migration033 candidate and an immutable borrowed initial-authority view. No terminal owner or production route is enabled. SQL028–032 and admission019 remain unchanged. The separately qualified terminal protocol is `aq-phase-terminal-protocol0.1.0`, source `baabe2e`, archive SHA256 `1b3e4afbd9356f6d7027b0a4be3fe37e2231b55bdfa10269c87a84bf9568f0bb`.

The view preserves distinct receipt kinds. A legacy identity hashes the actual canonical stored legacy response; a phase identity is the actual validated state32 ExecutionReceipt digest. The view has no Serde implementation and returns historical grant data, not a delivery sample or live permission. The legacy adapter still refuses accepted no-launch and phase execution origins.

033 classifies phase origin using actual030accepted ownership as well as032Grant ownership. Missing/invisible032 history does not fall through to legacy. An exact receipt-bound transaction-local writer marker, actual phase row, envelope/body/ACK associations and original atomic/immutable guards are required for a terminal write. An old open connection cannot gain that marker from defaults or a previous pooled transaction. This compatibility fence does not protect against a malicious database administrator.

The old closure foreign key into legacy execution receipts is replaced by strict origin-specific resolution of the actual owning table. The phase branch reads the real phase receipt JSON directly. No fake legacy row or GrantedLaunch is introduced. Legacy closure quota accounting remains independent of phase cleanup capacity.

A new immutable capacity row reserves one MiB per actual phase Grant before its transaction can commit. The first profile permits256MiB globally and16MiB per node, corresponding to256/16 retained reservations. Existing state32 records are backfilled transactionally; an over-budget migration refuses without relabeling or dropping data. The reservation is a logical admission budget, not preallocated disk. It is retained after stop/closure until a separately qualified settlement/retention owner permits reclamation.

The conservative bound uses every declared payload maximum, even allowing all three event rows together although qualified closure and Started are mutually exclusive:

| Retained columns | Upper bound |
| --- | ---: |
| Started original request +original receipt | 32KiB |
| Stopped original request +proof +original receipt | 48KiB |
| Closure original request +context +original receipt +binding JSONB +verifier JSONB | 124KiB |
| Per event: phase request +phase ACK | 128KiB |
| Per event: qualification JSONB | 4KiB |
| Per event: origin/request digests +two generated raw digests +kind | 274bytes |
| Existing stored digest strings across all three events | 448bytes |
| Total across all three event maxima | 615670bytes |
| Logical reservation | 1048576bytes |

Actual consumption uses `octet_length` over retained bytea/JSONB representations and these stored digest strings. Fixed UUID/numeric identifiers, row/index overhead and WAL are outside this logical metric. Per-attempt counting touches at most the three terminal rows. Grant reservations are globally bounded. No history scan or scale-insensitive latency claim follows from those bounds.

A test-only scoped Store factory opens the actual fixed migration prefix32 through the normal pool initialization. It never rewrites metadata, never chooses an arbitrary production profile, and does not affect spawned tasks or normal Store startup. Its large fixture future is boxed before entering the task-local scope. This permits genuine old-connection→manual033 and later ordered32→33 tests after registration.

Stage A qualification retains manual-prefix PostgreSQL tests separately from future ordered Store registration. The candidate remains unregistered until paired Started→Stopped and no-Started closure owners, mandatory-object validation and the full migration/writer/byte-bound matrix pass. Historical lookup is independently owned and returns no execution clock or live wrapper. None/absent history is never a NeverGranted proof.
