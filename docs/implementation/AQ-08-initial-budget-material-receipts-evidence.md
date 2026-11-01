# Initial grant budget and installation receipt qualification

Source `f5814b5d0a586f46dcf559f22d43af98cd93dfa0` caps the initial execution
deadline with the actual pinned remaining lineage elapsed-time policy and
retains installed-material claims atomically with the grant. No migration or
public wire changes are included.

The [manifest](AQ-08-initial-budget-material-receipts-evidence.json) pins 19 source
files and seven retained logs. Seven actual disposable-PostgreSQL native targets
passed **57 tests**, with no ignores, including both pinned actual-Git cases.
The two policy helper tests passed separately. All four new receipt cases are a
subset of the 57, not additional passes. Strict server all-target/all-feature
Clippy, formatting and source diff checks passed.

`admission_material_receipts` now retains the exact typed receipt, its original
material-fetch/install operation ID, and canonical receipt digest in the same
transaction as the grant. Exact Prepare replay requires the actual retained
identical body; missing or changed operation/digest/private binding refuses
without reconstruction. The shared historical reader uses the original grant
issuance time, preserving installation history independently of later retrieval
lease expiry. It does not grant current authority or remotely observe worker
private files.

Accept and Grant share one actual retained JSONB-text quota aggregate. Receipt
bytes join through their leases to establish node ownership. First Grant inserts
or checks the immutable receipt under the shared quota lock before summing:
new rows are charged once; exact existing rows are not duplicated. A late
Prepare-ledger failure and a receipt-byte quota refusal both roll back the new
row and leave the original ungranted state. The quota test explicitly uses
synthetic historical padding that is not a usable installation receipt; unusable
retained bytes must still count.

The retained initial budget preflight failure is intentional evidence of the
immutable profile boundary: modifying an already published v2 identity was
refused. The corrected fixture publishes a distinct v3 catalog/profile and
passes both the retained positive preflight and final suite. No failed run is
counted as qualification.

Actual-Git cases use executable SHA256
`c2ff26f5e63008aec0fbd4082a547e87825e651d4aba0744c4b9fdf752d58c39`, producer
`2035654eeda57ded8880ea3f518ff0b4d324fa49`, and `/usr/bin/git` 2.43.0.
Graph/material/worker installation claims remain bounded synthetic local
fixtures. Native physical execution, real provider/notification delivery,
production deployment and physical storage/performance closure remain outside
this evidence. Exact raw cargo logs retain their original trailing blank lines;
evidence-only staged EOF-whitespace warnings do not alter their hashes.
