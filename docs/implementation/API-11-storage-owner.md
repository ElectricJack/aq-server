# API-11 internal storage owner: migration 36 review boundary

Frozen owner source `896d566` (on `fda0a9d`, `acc25ae`, `07dc814`) passed three
actual migration/startup groups in 14.81 seconds and fourteen actual result-owner,
PostgreSQL and managed-file cases in 16.20 seconds. The
[qualification record](evidence/api11-media-storage/qualification.json) pins the
source and scope; [test output](evidence/api11-media-storage/tests.log) retains the
bounded invocation. These qualify the internal owner, not the public HTTP port.
Affected strict Clippy passed in 23.51 seconds; its
[output](evidence/api11-media-storage/clippy.log) is retained with the record.

Internal owner qualification candidate. No HTTP/config/bootstrap/capability/UI
exposure and no live database migration. Root reserves 36 after frozen conversation 35. This adds
new canonical supplemental-media metadata to the existing PostgreSQL database;
the separate pure component, frozen report transport and execution-input store
are unchanged.

The internal `result_media` owner will split into `db`, `files`, `processing` and
`cleanup`. It is explicitly constructed from actual AppState, a private root and
stable storage ID; it is not registered in AppState/config or any listener yet.
Its human methods receive trusted `User`, not caller UUID alone. The authorization
key is id+issuer+subject plus current Owner. Original id/issuer/subject are pinned
in intent; email is presentation and an email-only update cannot strand recovery.
Recheck the same request authorization key before and after IO/waits. Role loss
or same UUID subject/issuer rebinding denies; no fresh identity is manufactured
from a known UUID. Internal bounded expiry cleanup holds the store capability
and operates only eligible owned files, without borrowing human/worker tokens.

## Migration and startup seam

`0036_result_media_owner.sql` introduces:

- `result_media_storage`: immutable singleton identity and opaque fingerprint
  of the actual opened root and lock-marker device/inode/owner. Pinning both
  prevents a replaced lock inode from starting a second owner while the old
  physical job still holds its original flock. An unconfigured server may have
  no row; a registered root cannot silently be replaced or deleted.
- `result_media_budgets`: installation/project byte reservations, physical
  retained charge and monotonic historical intent count. Installation is seeded
  empty. Only nested intent-owner triggers adjust charges; operator limits can
  change within fixed hard maxima without resetting history/accounting.
- `result_media_intents`: exact raw request SHA/bytes, original actor key,
  actual legacy report/revision/source binding, storage/policy/limits/deadlines,
  one fixed file generation, monotonic lease/processing state, immutable canonical
  receipt and bounded cleanup state. No result/task state is mirrored here.

Admission reserves 10 MiB before IO. Trigger accounting locks installation then
project rows and automatically converts/reduces charges on legal intent
transitions; Rust must not duplicate those counter writes. Per-report pending
and retained attachments are capped at eight. A retained tombstone still counts
toward that bounded attachment list. Historical counts never fall; original
requests/receipts cannot be deleted/truncated/relabelled.

Receipt transition requires the original current Owner authorization key and
exact report/actor/input/variant linkage. Actual source/receipt proof still comes
from `results::load_locked` and the file owner, not JSON constraints. Trigger
checks are additional corruption/state barriers, not filesystem attestations.

The startup guard verifies exact enabled trigger target/function/type/body,
identity PK/unique keys and required nullability, critical CHECK expressions,
named index columns/predicates and FK columns/actions, required installation budget, and aggregate
intent/counter agreement. Missing/disabled/drifted state is refused without
repair. Existing 35 readers refuse 36 before changing metadata. Fresh 36 and35→36
tests preserve the actual conversation identity and check refusal/drift; they
qualified against disposable databases only; the final candidate's results are
recorded separately after its newest destructive vectors pass.

## Physical ownership and IO state to implement next

Hold a no-follow directory descriptor, stable marker and exclusive filesystem
lock for the lifetime of the owner, including detached-from-HTTP physical jobs.
Validate the current configured path still names that same directory and marker
before/after IO. Private files/directories use 0600/0700 and generated UUID/fixed
variant names only. A different root fingerprint fails closed; relocation/restore
to a different root inode needs an explicit future administrative procedure.

Keep an RAII physical activity registry shared by owner clones: exclusive
receive/finalize writers and bounded content readers. Lease expiry alone does
not prove the OS thread/FD is gone. Cleanup cannot claim/release charge until
all physical jobs for that intent have ended. Registry guards keep the root lock
alive; a new owner after process death must acquire that same root lock first.

One immutable generation holds input and variants. Reuse verified owned bytes or
confirm removal of an uncommitted generation before regeneration; never write
two generations against one reservation. Fsync files/directory, publish by rename
without copying, then revalidate lease, same human authorization key, actual
legacy result, root identity and original deadlines before receipt commit.
Crashes preserve accounted files and explicit original-intent recovery. Late
lease completion cannot publish or clear a newer operation's lease/busy state.
Recovery repeats generation and both parent-directory fsyncs even when a prior
rename is already visible. The tested durability checkpoint must precede receipt
commit. Cleanup likewise fsyncs the parent on an already-absent generation,
covering interrupted unlink recovery before it can release charge. Test hooks
follow real file operations; they simulate interruption without claiming an
actual power-loss test.

PUT first validates original-bound byte length/SHA before acquiring a new lease.
Exact Received/Retained replay returns the original status without a file write,
lease/deadline mutation or second generation. A mismatched body conflicts with
the complete original row unchanged. A final root/marker check after SQL waits
refuses a root swap before Received commit; nonretained status also checks root
identity before it can advertise recovery.

Retained charge is at least logical input+variant bytes and covers observed
allocated file/directory blocks within the reservation. Only confirmed absence
under a cleanup generation releases it; failure keeps budget and retry state.
Invalid/rejected uploads can be cleaned once their physical job ends; other
unfinalized uploads wait for original expiry, retained bytes for retention expiry.
Preserve receipt/provenance tombstones after deletion. Listing availability is
a bounded observation of owned regular files and expected lengths, with no
media body read. It does not attest the current body digest. Probes run on the
blocking pool after the SQL snapshot commits, then a fresh authorization,
report-binding and immutable receipt fence runs before return. Content reads
always check exact bounded byte length and SHA before exposure; missing/corrupt
bytes never produce an image or permission to modify a retained receipt.

## Qualification gate before any route

Actual SQL/filesystem tests must cover immutable raw/target conflict and original
actor, budget races, exact report corruption/unsupported native coverage, storage
symlink/root/file swaps, each interrupted fsync/rename/receipt boundary, live
physical jobs across lease expiry, stale lease completion versus cleanup, role
loss and subject/issuer rebind during held IO, recoverable email-only update,
retention/missing/corruption and exact canonical receipt/byte identity.
No synthetic success receipt or fake physical byte fixture can qualify the
positive retention path. The public route/DTO proposal remains the closed
`human-result-media-1` contract for the UI lane, but stays unmounted until this
storage gate passes.
