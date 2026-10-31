# AQ08 artifact phase HTTP follow-up

Tests-only source `53024c8`, based on qualified artifact owner `ce1214f`, adds
four actual disposable-Postgres/router tests. All four new tests and all four
existing artifact tests pass; strict target Clippy and package formatting pass.
[Exact commands, source/log hashes and reported test timings](AQ-08-artifact-phases-qualification.json)
retain the focused proof.

Preparing fetches use the original accepted plan after offer expiry and refuse
at plan expiry. Granted fetches obey the active grant deadline while the plan
and retained blob remain live, leave the complete hot row/revisions unchanged,
and do not infer Started. Both phases refuse cancellation; Preparing also
refuses a missing current accept receipt. Refusals expose no retention header
or artifact bytes.

The runtime/material/guardian records are explicitly synthetic fixtures. This
adds no production code and does not qualify a real worker, process, provider,
model capacity or grant producer. Root retains the separate full-workspace
artifact-owner qualification; this follow-up runs only the focused tests.
