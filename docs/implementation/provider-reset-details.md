# Provider reset details implementation

Current deployment: [restored alongside Providers settings](provider-reset-restored-2026-11-12.md).

Implemented from [the plan](../superpowers/plans/2026-11-11-provider-reset-details.md)
in isolated server/dashboard worktrees based on the deployed swarm-controls
lineage. Three GPT-6.1 Terra sub-agents implemented collection, server contracts,
and dashboard changes; root reviewed their diffs and independently verified the
combined result.

## Delivered behavior

The existing host producer continues to read Claude's structured `/usage` control
and Codex's app-server `account/rateLimits/read` without sending model prompts.
Codex's authoritative available count survives missing, capped, or malformed
grant details. Detail rows exclude spent/unqualified statuses, redact opaque IDs,
and preserve display labels and expiry. Missing or invalid reset data is unknown,
never a fabricated zero. A successful response without reset metadata clears an
older count; failures and Claude cache fallback preserve the original independent
reset observation time.

The additive `ProviderUsage.reset_credits` contract carries count, optional
display-only grant details and its own observation. Snapshot parsing validates
account binding and bounds, and malformed reset metadata cannot discard valid
quota windows. Legacy snapshots work with an unknown reset count. The canonical
schema can now be generated through the small contract crate's example, using the
same `schema_bytes()` function as the existing server generator.

The dashboard displays reset information per account inside the existing usage
disclosure. Counts, zero, unknown, unavailable, stale observations, expiry and
restrictions remain distinct. Expiration overrides a previously reported usable
flag. Provider limit keys become readable labels. No redemption UI was added.

## Claude qualification limitation

The installed Agent SDK is 0.3.321; the live native CLI resolved to 2.1.322 during
qualification. Its `usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET`
control with `skipBehaviors: true` returns plan windows. The declared control
request exposes only `skip_behaviors`. Inspection of the installed native client
showed both `get_usage` and slash `/usage` using the same plain/cached utilization
read; the separate `cedar_ember` grant-specific read occurs in an internal reset
flow, with no supported independently timestamped structured read control.

A live SDK response's incidental `rate_limits.cedar_ember: null` cannot establish
that an account owns zero resets. Cached utilization can also lack an independent
grant observation time. The producer therefore deliberately returns **unknown**
for Claude resets while preserving real Claude quota windows. This implementation
does not claim live Claude reset-count support. No private HTTP requests, raw
OAuth extraction, active-session input, terminal reset-menu automation, or reset
redemption were used to work around that limitation.

## Root verification

Root independently ran:

- `python3 -m unittest scripts/test_provider_usage.py`: 9 passed, including a
  root-added regression for spent/unknown detail statuses and authoritative count.
- `timeout 900 cargo test --locked -p aq-provider-quota -p aq-dashboard-contract`:
  8 quota tests and 4 contract tests passed; doctests passed.
- `cargo fmt --all -- --check`.
- `timeout 900 cargo clippy --locked -p aq-provider-quota -p aq-dashboard-contract --all-targets -- -D warnings`.
- Canonical schema generator `--check`, `protocol/dashboard-read/v1/check.py`,
  and `scripts/build-budget.py`.
- Dashboard adapter/component/hook/Shell tests: 81 passed; typecheck and lint.

Root also ran the updated producer once against both installed live clients,
writing only to a private temporary snapshot: both refreshed with one quota
window each; Codex returned known count **0**, Claude unknown/null. The running
timer and its snapshot were not changed.

A separate deterministic fixture passed through the actual Python normalizer,
Rust snapshot collector and serializer, canonical JSON schema validator, and the
compiled React view. The authoritative Codex count of 3 survived a one-row detail
list, while Claude remained unknown and both windows showed 35% used. Only the
transport envelope version label was adapted to the preview's proposed-0 mock
bootstrap; the projected account payload was unchanged.

Root captured and opened the compiled preview's known/zero and unknown/stale
disclosures at 1440, 390 and 320 pixels, plus the cross-language fixture at 390.
The preview was explicitly labelled Mock data. No horizontal document overflow,
browser page errors, obscured close button or Escape/focus regression was found.
Evidence is retained in the dashboard repository at
`docs/implementation/evidence/provider-reset-details/`, including the served
manifest hash and the seven screenshots. Approved reference:
`docs/design/01-overview-v3.png`; delivered visual subset: account usage disclosure.
Full-page product/design acceptance is outside this increment.

## Operational status

Implementation and review are local to `feat/provider-reset-details` in the two
repositories. The dashboard pins the exact server contract commit. Production
build verification and contract-pin checks are recorded with the dashboard
implementation report. The running server, timer configuration, host producer
checkout, and dashboard bundle have not been updated. Claude live reset grants
remain unavailable through the chosen client interface.
