# AQ-25 pure wait engine qualification

Source `10dcfbd` passed seventeen pure tests in0.06s after a1.09s warm-target
rebuild. All-target strict Clippy passed in0.61s. An independent source reviewer
accepted the same freeze without running builds. The
[record](evidence/aq25-pure-engine/qualification.json) retains source hashes,
scope and [actual test output](evidence/aq25-pure-engine/tests.log).

`aq-wait-engine` emits bounded CAS transition plans using frozen CON03 types.
It installs no SQL registry, source subscription, enforced scheduling hold,
runtime capability, public WaitRegistered response or physical process state.
The qualified source has no server, SQL, HTTP, Tokio, worker, admission or
playbook-engine dependency. No external dependency version was added.

SourceBoundary retains the source OWNER snapshot digest. A separate internal
owner snapshot ArtifactRef/hash and AQ projection digest bind the trusted
adapter's decoded result independently. A digest is not an authority proof;
the actual adapter must qualify its owner's replay/snapshot semantics and
verify evidence-to-condition linkage. Tests use fake committed source owners,
not real Graph registration or mailbox/clock adapters.

The fake owner holds early allocated sequences while later transactions commit,
coalesces a transaction's final state, distinguishes a real historical terminal
commit from an internal complete/reopen operation, and returns replay gaps rather
than guessing current-only success. Installation always requires reobservation.
Restart tests preserve subscription identity, invalidate reclaimed work tickets,
retain dirty hints, and keep unavailable sources pending with bounded backoff.

Review corrections also bind restored work kind to phase/installation coverage,
bind immutable outcomes to retained member facts, and reject wire-valid but
unsupported terminal-unavailable/manual-override states, early deadlines and
cancelled resume. These are reducer invariants, not source authentication or
durable-storage qualification.

AQ-25 remains partial. A separately reviewed actual SQL owner/lock hierarchy,
AQ-08 hold enforcement, the qualified Graph observation protocol, WRK-16 parking
and AQ-26 wake/ack are required before end-to-end agent waits/hibernation work.
Migration38 remains unallocated. No provider, database, live fixture, live agent
or canonical service was read or changed by this qualification.
