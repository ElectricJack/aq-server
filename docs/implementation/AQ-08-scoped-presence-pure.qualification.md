# AQ-08 scoped presence pure contracts

Source `db1f5e7` freezes16 package paths. The candidate passed67 package tests
(58 preserved tests plus9 new presence tests), strict all-target package lint,
package formatting, independent Python canonical hashes/clock vectors and three
independent JSON-schema fixture validations. The frozen0.1.7/0.1.8 source and
artifact inventory remains unchanged, including README.md. New module docs are
SCOPED_PRESENCE.md. No archive was created; root owns the joint exact0.1.9 archive
with accepted-preparation v2 after both candidates freeze.

The new protocol separates explicit node-wide ProvisionableOnly inventory from
one-grant Present/Missing/Unknown facts. It includes exact receipt/private-record
hashes, actual observed digest/size, logical/kernel boot and read-after-permit
CLOCK_BOOTTIME intervals, immutable operation receipts and a conservative original
grant-issuance age bound. Structural validation never authenticates a producer,
creates global availability or authorizes execution. Actual daemon owners,
compatibility epochs, current authority and qualified physical worker I/O remain
separate gates.

Rerun commands below use the leased slot target and disable incremental builds;
see [Cargo worker build guidance](../guides/cargo-worker-builds.md).
Historical commands, results and hashes remain in the machine-readable evidence.

```sh
env CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p aq-admission --locked --offline
env CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo clippy -p aq-admission --all-targets --locked --offline -- -D warnings
cargo fmt --package aq-admission --check
python3 crates/aq-admission/tools/scoped_presence_vectors.py
```

The [machine-readable inventory](AQ-08-scoped-presence-pure.qualification.json)
hashes exact `git show db1f5e7:path` bytes and original logs. Each package path was
also compared byte-for-byte to the frozen source before final commands.

Two development failures remain retained: appending to the old README correctly
failed the immutable-source test, so the new text moved to a separate document;
a Unicode-boundary test attempted to construct an already-invalid Text before
calling the decoder, so it now tests actual incoming JSON at512/516 UTF8 bytes.
Neither old validators nor immutable inventories were weakened to pass. Raw log
whitespace is retained. No provider call, paid execution or external deployment
occurred.
