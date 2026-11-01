# Actual format10 phase host/outbox union

Server assertion source `f533e20` on `3eb5d59` and worker main `e805bb4` join the
format10 paired local outbox with the previously qualified schema32 process path.
The only server changes are three expected-format assertions (9→10) and explicit
local Started/Stopped outbox checks. No handler, migration or production route
changes. The earlier [format9 evidence](../AQ-08-phase-host-process-evidence/qualification.md)
remains unchanged.

The freshly built independent worker registers, receives actual phase authority
over daemon HTTP/PostgreSQL, downloads38 artifacts/one material, hydrates through
pinned aq-git/Git, and launches the sealed synthetic ELF through the production
helper and tmux3.4. The physical process passes6.60s. Actual Running and ECHILD
stop are durable, with stop recorded at revision34, and the wrapped local publications match the
actual execution/accepted receipt digests, protocol artifact, operation IDs,
guardian/pane proof, and their respective physical commit revisions. Both remote
receipt fields are explicitly null: this qualifies local queued facts, not remote
phase lifecycle acknowledgment.

With the same new pinned worker/test executable, execution-only passes5.74s and
the input/original-expiry case passes7.01s. Existing binary tests2 pass. Strict
Clippy passes for the affected server test target (32.50s) and worker smoke binary
(11.39s); formatting passes. The worker release build takes72s, server test build
56.98s. These are finite test-process/build timings on the shared WSL2 host, not
service latency or fleet capacity. A quiet run window was coordinated; policies,
5s availability age and execution lease were not widened.

The test executable and release worker were copied to a separate private pin
directory before invocation; hashes were verified and those copies were executed
directly. Worker SHA256:
`c52c22046f7ad05a99a0e762cd580714b17dca9af8c2c9ac98140b3887925bf7`.
Server test executable SHA256:
`66f93c01d23ec9850132f119b480c2350c820769ca2676fd8b0d4d172d150011`.
This avoids shared Cargo target replacement during the qualification run.

Registry/provider/Graph and runtime/harness eligibility remain synthetic. No real
provider call, default capability, renewal, no-Started closure publication, task
completion, source/effect settlement or hold release is qualified here. The full
paired daemon owner/ACK is a separate lane. [Exact hashes, commands, raw logs and
measurements](qualification.json) are retained next to this report.
