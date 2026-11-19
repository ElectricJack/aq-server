---
title: Hand-started sessions lease and write back the Codex login on the shared store; the server checks every write-back
author: Jack
date: 2026-11-19
branch: feat/session-codex-login
cost: { minutes: 758, tokens: 1658281 }
id: l-67b0b0af
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/manual_agents/session_login.rs
  - crates/aq-login-store/src/lib.rs
  - crates/aq-login-store/src/codex.rs
  - crates/aq-server/src/profiled_execution/login_refresh.rs
  - crates/aq-swarm-settings/src/providers.rs
---

## Decided
manual_agents/session_login.rs adds POST /v1/worker/manual-agents/{id}/login (newest stored login of the provider in the session's own spec, aq-material-version; only the node that owns the session, desired_running, not pending, not stopped) and .../login/refresh?base_version=N (status running or stopping; compare-and-swap on the same lineage as the attempt route, origin refresh with no lease_id, 409 {accepted:false,version} when N is not current). Both write-back routes now go through login_refresh::refresh_of_head: for provider openai the bytes must be a well-formed ChatGPT login (aq_login_store::codex::stamp, API-key logins are not), the same account_id as the stored head, with a later last_refresh. The owner's age limit is provider_settings.codex_login_max_age_days (migration 0149, Settings > Providers); login_refresh::lease_refusal checks it at the attempt's material fetch and at the session lease and answers 409 'codex_login_stale: stored Codex login is N days old (limit M): import your current login'. import-login refuses a file that is not a ChatGPT login for an openai credential. /api/worker-logins accepts only [anthropic].

## Stale login and selection
aq_login_store::mask_stale runs beside aq_swarm_settings::providers::mask in intelligence/resolver.rs (resolve and the fence): a Codex credential whose newest stored login is past the limit gets provisionable=false, which aq-selection reads as MissingCredentials and falls back on. The fence uses the decision's own now_ms and an owned attempt keeps its frozen value. swarm_settings.rs puts the reason on the provider (attention) for the dashboard. There is no owner notification: aq-notifications delivers task messages only.

## Why a new crate
scripts/build-budget.py refuses any growth of the aq-server lib against origin/main. The lineage core (Lineage, lineage_locked, append_locked, store.rs, codex.rs) moved to crates/aq-login-store and login_refresh re-exports it, which pays for the new routes.

## Watch out
Migration 0149 replaces the CHECK that tied origin='refresh' to a lease_id: a session write-back has none. The fixture in tests/it_native/subscription_refresh.rs seals its selection to provider fixture-a and intelligence_attempt_selections is immutable, so an attempt cannot name openai end to end there: setup_as changes only the configured credential's provider (enough for write_back, which reads the registry), and the fetch-side age check is tested through lease_refusal directly.
