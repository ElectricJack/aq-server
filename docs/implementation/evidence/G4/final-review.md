# G4 final ordinary review

Reviewed the combined feature against fetched `origin/main` at
`841e19ff6260ce7a97c600227e94c3c42c1abec5`, with recovered source head
`a524c03e8b1ac137daddc3e5ea6f42a748f8c759`. This is a controller self-review,
supplementing the three independent slice reviews archived in
`slice-reviews.md`; it is not a cross-family adversarial review. G4 has no
`review: adversarial` marker in the task contract.

The final contract step adds examples and validation to the previously reviewed
routes; its only Rust change advances the Graph search fixture's fake clock
before snapshot insertion. The production cache eviction and 403 assertion are
unchanged. The generated schema contains typed attention, outcomes, grouped hits,
account quota, additional attention kinds and reference-only digest revisions.
Prior numeric-revision read fixtures remain accepted; invalid revisions,
percentages, enums and extra fields remain rejected by the fixture checker.

The combined route wiring uses the existing current-principal fence, preserves
the optional review and playbook owners, and keeps Graph/result/playbook reads
behind their existing owner checks. Source failure contributes unknown totals
and partial coverage. Graph readiness, current attempt/process observation,
review eligibility, retained results and settlement receipts retain separate
semantics. Snapshot continuation binds the actor, authorization partition,
scope, query, types and page size, and rechecks bindings/current session access.
Collector authentication stays server-side and opt-in, uses a fixed endpoint,
rejects redirects, bounds file/network reads and preserves last-success times.
No new blocking defect was found in these paths.

## Disposition of archived minor findings

All four findings remain nonblocking, as in their original slice reviews:

- **Deferred: unexpected quota task panic.** Normal upstream/authentication/
  parse/timeout failures return stale or unknown account windows. An unexpected
  spawned-task panic can omit that account while marking the response partial.
  The collector contains no deliberate panic/unwrap in its production path;
  retaining an external fallback identity would improve exceptional recovery.
  This exception is recorded as a remaining implementation limit.
- **Rebutted as a correctness issue: opaque additional-window labels.** Stable
  IDs distinguish every observed window and numeric/freshness semantics are
  preserved. Hash-based labels intentionally avoid forwarding arbitrary upstream
  text; more descriptive sanitized labels would be a usability improvement.
- **Deferred: overview freshness boundary regression.** The delivery fence
  explicitly rejects both time before observation and time at/after expiry.
  Existing tests cover owner observations and post-await binding mutation.
  Additional fake-clock boundary tests would strengthen coverage; inspection
  found no defect in the predicate.
- **Deferred: direct stale-summary regression.** Both matching and excerpt
  selection discard a summary whose revision differs from content revision.
  Existing Graph tests cover sanitized summary excerpts and metadata-only
  matching exclusion. A direct stale-summary-only fixture would improve coverage.

No live-account provider interoperability, downstream dashboard qualification,
or full CI suite is claimed. Unsupported question/blocked-task owners are
explicitly unavailable rather than fabricated. The cross-repository consumer
follow-up is recorded on `steady-cascade-45.5`; G5 owns that consumer work.

Final resource-admitted verification and source/base references are recorded in
`../../G4.md`. Publication must freeze the reviewed source tree and retain the
schema digest; the final commit SHA is recorded in the AQ completion evidence.
