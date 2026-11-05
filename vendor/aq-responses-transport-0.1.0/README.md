# aq-responses-transport

Owner-scoped stateless Responses request policy and bounded HTTP/SSE forwarding.
Consumes an existing numeric-loopback listening socket and a parent-owned
retained-credential callback. Uses shared Digest/ModelSettings contracts; has no
worker, host, daemon, admission or material-store dependency.

The production endpoint is fixed HTTPS OpenAI Responses. Offline tests use
explicit numeric-loopback fixtures only. A local capability and immutable policy
digest grant no launch authority. Per-response output limits do not enforce an
aggregate cost/token budget. `cancel()` triggers denial; await `stop()` before
claiming completed forwarding closure. A canceled filesystem wait does not
forcibly stop its blocking read.
Eight shared nonwaiting read permits bound actual blocking callbacks, including
reads that outlive a canceled/expired async wait. A permit returns only when the
physical callback ends.

This package is currently workspace-only (`publish=false`). See
`docs/implementation/WRK-17-responses-transport-extraction.md` at workspace root
for worker compatibility and qualification evidence.
