# Bounded central expiry of never-granted offers

Configured native execution now starts a central housekeeping loop that invokes
the existing transactional NeverGranted owner. Without profiled execution
configuration the loop returns immediately. No worker connection or additional
service is needed to expire an offline worker's unaccepted offer.

Each tick selects at most eight held Offered/Preparing attempts with no hot grant,
in attempt-ID keyset order. A disposable cursor advances for every attempted
closure, including refusal, and resets at the end. This prevents an uncertain
first item from monopolizing every page; it is not a scheduling priority or a
durable owner record. A page can include fresh offers, which the existing owner
leaves held. Process restart can safely begin the scan again.

Ticks run every second without overlap or catch-up bursts. One quantum is bounded
at four seconds; selection statements have a two-second SQL limit. The closure
owner has a250ms lock timeout and two-second per-statement limit. Cancellation or
timeout can leave the disposition unknown to the loop; the transactional closure
receipt and original held-claim state remain authoritative. No success is inferred
from a returned timeout, and subsequent scans do not replace operation history.
Incomplete sweeps produce a generic warning at most once per30seconds plus debug
counts. Result counts and timeouts do not establish constant-time SQL scans or a
guaranteed throughput/cleanup latency.

Selection is only a hint. The exact existing SQL/Rust owner validates original
offer/preparation deadlines, absence of any grant, actual material/source/effect
records and immutable closure proof while holding authority locks. A concurrent
grant, malformed plan, uncertain effect or missing owner refuses closure. This
slice does not implement granted-without-Started recovery, infer physical stop,
settle a result or release a worker's local workspace/resources. Local release
and historical reconciliation still require their own admitted receipts.

Shutdown stops or cancels this disposable background task. There is no background
replacement worker or host launch. Source configuration, native offer eligibility
and finite no-grant retry limits remain with their existing owners. No new schema
or public protocol is introduced.
