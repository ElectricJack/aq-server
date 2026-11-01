# Ordered accepted-preparation schema30 qualification

Source `66e83be`, with narrow proposal-auth fix `dd441e0`, follows actual registered
029. Normal Store migrations install30 and align source/readable/Docker metadata.
The previously unregistered candidate remains historical; the final SQL changes
only by adding its explicit metadata30 transition before registration.

[Machine evidence](AQ-08-ordered30-evidence/report.json) pins the selected source
closure and tracked logs. Final owner/scope/schema tests pass19; related12 targets
pass84 with4 declared ignores; metadata invariant passes1; explicit pinned actual
Git v2 source test passes1. These groups cover105 distinct passing cases, with
three other ignored external-process/Git cases not run in this qualification.
Strict all-target/all-feature server lint, package formatting and whitespace pass.
The isolated build target is `/tmp/aq-phase30-qualification-target`.

Real29→30 collision and a failure after SQL metadata transition roll back both
metadata and new tables; explicit retry succeeds. Current30 missing one or both
tables refuses reopen without repair, and already-open legacy admission refuses
missing phase tables rather than undercharging. Older29 refuses30. Fresh storage
is empty and migrations alone activate no owner. Fixtures use actual Store
installation and assert required tables rather than manually applying SQL.

The cross-node regression first reproduced an actual unauthorized proposal row
from a second enrolled/registered node. The fix validates full remote node/boot/
attempt scope before foreign-owner SQL. The final ordered case and an independent
parent-run profile29 case pass; before-fix evidence is retained.

One real-clock route/auth/nonce run safely refused409 under build load. The
functional middleware test now freezes its fixture clock after actual NativeOffer
publication but before first v2 Proposal/Accept, preserving original documents.
It proves route/auth/nonce behavior only, not freshness under real I/O or latency.
All other expiry/barrier tests and the actual Git probe keep their clock semantics.
The real Git probe reads the same retained source after proposal expiry, verifies
digests/bundle, and refuses reads after actual export retirement. Historical source
eligibility itself had not expired; no fresh eligibility receipt is fabricated.

The owner stays default inactive. No new execution Grant, phase worker input/host
adoption, runtime deployment, provider/notification call or PB04/general cold-work
closure is claimed. Unknown/expired phase attempts remain held.
