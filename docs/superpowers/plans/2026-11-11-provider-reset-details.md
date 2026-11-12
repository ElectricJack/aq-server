# Provider reset details

## Outcome and current deployment

Show available redeemable usage resets for Claude and Codex inside the existing
account-wide usage disclosure. Preserve each provider's actual window reset times,
account identity, grant restrictions, and observation timestamps.

The running MS1 server is built from the swarm-controls server lineage
(`fdb4850159a419526bf2fb44f54474952c47c56e`). Its host timer runs
`scripts/refresh-provider-usage.py` once a minute. Claude is read through the
Agent SDK's structured `/usage` control, with an account-bound CLI cache fallback.
Codex is read through app-server `account/rateLimits/read`, the structured account
limit interface corresponding to the CLI status display. The producer atomically
writes a sanitized snapshot mounted at `/var/lib/aq/provider-usage`; the server
projects it through `/api/v1/provider-usage`. Both providers are already live.

Work in isolated `usage-resets-server` and `usage-resets-dashboard` worktrees,
based on the deployed lineage, without changing the running services during
implementation. User explicitly requests Terra sub-agents and root verification.

## Collection and normalization

Keep collection client-owned: no model prompt, raw OAuth extraction, private HTTP
adapter, or input injected into a running agent session. Preserve Codex's
`rateLimitResetCredits.availableCount` as authoritative even if its detail list
is null, empty, or capped. Optional details carry label, expiration, remaining
count if known, usability if known, and affected limits.

Investigate Claude's installed client/SDK for a structured grant read. Its current
declared usage response has quota windows but no declared reset grants. Qualify
the actual supported control response before choosing a read path; do not guess
that ordinary window reset times represent banked resets. If grant metadata is
not exposed by the installed client, publish an explicit unknown result, document
the limitation and exact investigated interface, and keep the existing usage
collection healthy. A missing grant block must never become a known zero.

Normalize a per-account snapshot field `reset_credits` with this shape:

```json
{
  "state": "known",
  "available_count": 2,
  "grants": [{
    "label": "Weekly reset",
    "remaining_count": 1,
    "expires_at": "2026-12-01T00:00:00.000Z",
    "usable_now": true,
    "clears": ["seven_day"]
  }],
  "observed_at_ms": 1794438000000
}
```

`state` is `known`, `unknown`, or `unavailable`; `available_count` is a bounded
nonnegative integer only when known, otherwise null. `grants` is nullable: null
means details were not provided, while an empty list is a provided empty detail
list. Grant fields are bounded display data, not opaque credentials or redemption
tokens. `remaining_count`, `expires_at`, and `usable_now` are nullable when unknown.
Reject invalid individual reset metadata without discarding valid usage windows.
Expired, paused, future, or conditional Claude grants must not be represented as
unconditionally usable. Preserve ownership count versus usability where known.

An actual source failure may retain its previous observation with its original
timestamp. A successful response that omits reset metadata replaces that component
with unknown, so an old count never silently becomes fresh. Quota-cache fallback
must not manufacture reset metadata from an older successful SDK read.

## Server contract

Add optional `reset_credits` to `ProviderUsage`, with the same display fields but
an `observation` object replacing `observed_at_ms`. Reuse the existing observation
source, quality, completeness and server-clock freshness conventions. Preserve
reset observations independently of window observations. Old snapshots and clients
remain usable. Update the canonical schema/fixtures through existing generators,
then synchronize the dashboard's pinned contract and wire adapter.

## Dashboard

Show an account's reset section within the existing usage disclosure, leaving the
compact header composition intact. Render count, available grant labels, expiry,
and usage restrictions when supplied. Distinguish known zero, unknown/unavailable,
and stale values. A failed refresh must mark reset observations stale alongside
windows; time-based aging must work between polls. Do not add redemption controls.
Follow the existing approved shell/Overview reference, tokens and disclosure
patterns, including keyboard closure and 320px/390px reflow.

## Execution ownership

1. Terra collector agent: client response qualification, Python/JS producer and
   focused normalization/failure tests; edits only producer scripts/tests.
2. Terra server agent: Rust contract, snapshot projection/cache and canonical
   schema/fixtures; focused Rust/contract checks, no new integration-test binaries.
3. Terra dashboard agent: wire/view adapter, freshness, account disclosure,
   fixtures and focused UI tests, design-reference review and preview evidence.
4. Root: inspect each diff, resolve contract mismatches, independently rerun focused
   checks, exercise the combined producer/server/dashboard boundary, inspect desktop
   and phone screenshots and record actual remaining limitations.

## Acceptance and validation

- Codex known count is retained when details are absent/capped; zero differs from
  missing, malformed, unsupported, or failed reads.
- Claude grant support is evidence-based; unavailable metadata is explicit and
  never breaks real Claude usage windows. Cover supported grant normalization if
  the installed client exposes it.
- Failures, cache fallback, response omissions, account separation, expiration,
  malformed counts/timestamps, bounded data, and independent freshness are tested.
- Run producer unit tests, focused `aq-provider-quota`/`aq-dashboard-contract` tests,
  canonical contract validation, dashboard adapter/component/hook tests, typecheck,
  lint/build as applicable. Every cargo test has a timeout and a private target.
- Root captures and opens the actual compiled preview at desktop and phone sizes,
  verifies the counts/unknown/stale disclosure states and lack of overflow, and
  records whether the preview uses labelled fixtures or live API data.
- No reset credit is consumed as part of validation.
- Record implementation results, test commands and qualification limitations next
  to this plan. Deployment is a separate operational step after a reviewable result.
