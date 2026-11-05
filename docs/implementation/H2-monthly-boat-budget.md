# Monthly Boat VM-time budget

Task: `grand-vault-92`.

The owner's recorded decision is the $20/month Boat plan with an **$18/month
AQ VM-time ceiling**, concurrency 4, small VMs by default, auto top-up and extra
starts off, and saved snapshots within the included allowance. Source:
`docs/research/2026-11-02-v2-build-supervision-log.md:394–398`.

This replaces the `sound-vault-88` interim note that `spend_ceiling` only saw
token costs and could not enforce VM-time spending. Configure the Boat node's
existing advertisement with `spend_ceiling = { currency = "USD", micros =
18000000 }` and `max_concurrent_sandboxes = 4`. These are the worker's existing
configuration fields; no advertisement or published protocol schema changes.

## Rates and window

`aq_accounting::vm_time` adds serializable size rates, lifecycle facts and a
monthly budget. The server pins `boat-2026-11-03` to each sandbox's first fact:

| Size | USD per running sandbox-hour | Microunits/hour |
| --- | ---: | ---: |
| small | 0.018 | 18000 |
| default | 0.036 | 36000 |
| large | 0.072 | 72000 |
| xlarge | 0.200 | 200000 |

These are the approved 0005 §5.2 amendment facts, not live vendor quotes.
Each interval is intersected with the current UTC calendar month, including
December rollover and leap February. Integer arithmetic multiplies the running
milliseconds by the hourly rate and rounds up once per sandbox/month to a USD
microunit. Stopped/archived time is free. Running facts accrue through the
placement read's clock, even after node loss or grant expiry. Replayed reports
replace the same interval rather than appending a second cost.

## Controller reporting and rollout

The node controller posts JSON to the worker listener at
`POST /v1/worker/observations/vm-time`, using its node bearer credential:

```json
{
  "attempt_id": "11111111-1111-4111-8111-111111111111",
  "sandbox_id": "boat-sandbox-id",
  "size": "small",
  "started_at_ms": 1793491200000,
  "stopped_at_ms": null
}
```

Report the actual VM creation time and selected size, including paid
preparation before the grant. Persist the report in the controller journal and
retry it after reconnecting. Once the backend confirms stop/archive or
destruction, resend the same identity, size and start with `stopped_at_ms` set
to the confirmed end. Both initial and stop reports are idempotent. Start and
size are immutable; a committed stop cannot be removed or changed. A VM that
was never created may be represented by a zero-duration fact with a stable
`not-created/<attempt-id>` sandbox ID, only after the controller proves absence.
The endpoint accepts only a registered Boat node reporting its own attempt.
Subscription credentials and delegated runner permits cannot report VM facts.

**The aq-worker controller must adopt this reporting endpoint.** This server
task supplies the endpoint and enforcement, not an aq-worker release. Backfill
historical lifecycle facts for the node before enabling its monthly ceiling.
Every attempt without a fact makes the node unaccounted; do not fabricate a
zero to make an old controller pass. The fence excludes its own newly reserved
attempt from this coverage check because that VM may not yet exist. The node's
other missing attempts still refuse. There is no synthetic lifetime-token
fallback for Boat.

Selection, preferred-peer eligibility and the existing admission fence consume
the monthly spend. At or above the ceiling, they refuse with
`node_spend_ceiling_reached`. Missing evidence, read errors, unsupported currency
or the bounded read's overflow refuse with `node_spend_unaccounted`. A known
lower bound already at the ceiling can refuse even if other facts are missing.
With no historical attempts, an empty VM ledger is a known zero. Other backend
kinds keep their existing lifetime token accounting.

This is placement admission, not a promise to cap an in-flight vendor invoice.
Already running VMs keep accruing until they stop under the existing lifecycle
and deadline controls. Fixed plan charges, snapshot extras and provider token
or subscription costs are not VM-time costs; keep the vendor-side limits above.

## Compatibility and verification

Migration `0062_vm_time_accounting.sql` adds `accounting_vm_time`; old token
facts and advertisements remain valid. State format and image label advance to
62. Rate cards are pinned in the ledger, so later price changes do not silently
reprice retained intervals.

Run the focused checks with the disposable PostgreSQL available:

```sh
DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres aq test scripts/test-monthly-vm-budget.py -s
```

The wrapper limits Cargo to two build jobs and one test thread. It checks the
accounting crate, month boundaries, placement and admission rechecks, the new
authenticated report/ceiling test, existing provider-call accounting, store
migrations and image state labels. It performs no Boat network calls or sleeps.
