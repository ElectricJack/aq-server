# Scoped presence isolation and schema-health followup

This bounded followup to7a19e0a adds four deterministic tests and a narrow Store
reopen validator. It changes no wire contracts, migration bytes, admission limits,
placement policy or running authority behavior.

One test creates two relational presence scopes on the same node. A uses actual
publication/Accept/material/Grant/report owners. B is an explicitly synthetic,
structurally valid Offered snapshot plus a pending lease and storage head; it has
no task claim, sealed launch, hot authority, installation acknowledgment or owner
journal. The test qualifies SQL head isolation, not public multi-slot placement
or B's authority to run. A newer Missing for A, exact old A replay and a compatible
same-set inventory refresh leave B's Present/head unchanged. The test also checks
that the real hot state, owner journals and verified-release rows do not change.

Controlled HTTP request-body trailer barriers occupy both inventory admission
slots or all four report admission slots before running other actual requests.
Inventory saturation leaves report+renewal progress; report saturation leaves
inventory+renewal progress. Holding both presence PG connections leaves renewal
progress on its independent pool. No sleeps, broad load generator, throughput or
fleet-capacity claims are involved.

Store reopening a current schema31 now checks for the binding-set JSONB column
and exact normalization function signature, refusing missing storage as corrupt
without silent repair. The schema test removes each independently and verifies
both refusal and continued absence. Existing ordered30→31 rollback and semantic
projection cases remain passing. This is a narrow missing-object check, not a
comprehensive schema/body attestation or database-administrator boundary change.

Physical worker presence production, provider execution, public multi-slot
scheduling and sustained load remain separate qualification. No provider calls or
external notifications occur.
