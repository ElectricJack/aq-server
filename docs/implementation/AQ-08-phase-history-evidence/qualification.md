# Authenticated phase history lookup

Source `c8af391` (integrated `d11f6b4`) adds the explicit
`phase_terminal::history::read` owner. It uses the frozen phase terminal0.1.0
contract, current node authentication and actual retained032 issuance/030
acceptance. A worker can query using its known binding, accepted receipt digest
and Prepare operation even when its first execution reply was lost.

Four actual PostgreSQL tests pass in7.95seconds with one test thread. They cover
absence before issuance followed by the same lookup returning known identity;
expired execution and retired runtime eligibility; no hot state/journal/hold or
lifecycle effects; exact original Prepared checkpoint; wrong origin, operation,
binding and node; credential rotation; corrupt ownership/receipt and missing
storage; and backwards projection time. Two deliberately blocked SQL readers
consume the two history admissions, a third is refused, and the independent
renewal connection pool remains available. Strict lint for the changed library
and test target and formatting pass.

Reads take the current node lock before the exact attempt, then validate the
retained phase chain and original registration. They use only the General SQL
pool, two process-wide request permits, a3second total budget,500ms lock timeout
and2second statement timeout. The response is bounded to64KiB. Missing history is
a projection snapshot that may later advance; it never proves NeverGranted.
Neither identity nor projection time is an ExecutionDelivery or live permit.
Corrupt existing ownership is refused rather than hidden as absence.

This mounts no production route and performs no lifecycle mutation, renewal,
settlement, release or host recovery. The fixtures use actual SQL/authentication
owners with synthetic Graph/Git/runtime evidence. The HTTP transport and complete
paired lifecycle remain separate qualification. [Source hashes and raw checks](qualification.json)
are retained here; timings describe this test profile only.
