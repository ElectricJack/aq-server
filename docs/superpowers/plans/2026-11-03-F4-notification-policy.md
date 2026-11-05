# F4 notification policy — implementation plan

**Goal:** automatic owner notifications for six attention events, one per
episode, flood-bounded, through the existing `notifications::persistence::Service`.

**Architecture:** a new `notifications::policy` module owns three tables
(migration 57): `notification_episodes` (episode key = kind + project + subject,
generation counter, open flag, the exact `SendRequest` it submitted),
`notification_policy_events` (source-event replay receipts) and
`notification_flood_windows` (per-scope 60 s windows: notified count, collapsed
count, frozen summary request). The policy commits its own transaction first,
then submits the stored `SendRequest` through `Service::enqueue` (idempotent on
derived `operation_id`/`message_id`). A replayed source event re-submits the
stored request, which the notification store answers from its receipt, so no
second intent is created. The runtime loop flushes closed windows into one
count-carrying summary message.

**Tech:** Rust, sqlx/PostgreSQL, existing `TestDb` fixture, fake transport only.

## Tasks

1. **Failing tests** (`crates/aq-server/tests/notification_policy.rs`):
   - `credential_invalid_notifies_once_per_episode`: open → notified (1 message);
     second open (new source event) → `already_open`; replay of the first source
     event → identical response, no new message/operation; resolve → reopen →
     second message; other profile → independent episode.
   - `flood_of_failures_collapses_to_bounded_notifications`: 50 task failures in
     one window → 3 messages; flush before window end → none; after → exactly one
     summary carrying `47`; second flush → none.
   - `no_external_route_is_reported_not_claimed`: unconfigured store →
     `no_external_route`.
2. **Migration 0057** + `STATE_FORMAT_VERSION`/`READABLE_STATE_FORMATS`/Dockerfile
   bump to 57.
3. **`policy.rs`:** `Kind` (6 kinds; `question.asked` is the F5 event kind),
   `Transition`, `Event`, `Policy::observe`, `Policy::flush`, derived IDs
   (`sha256` → RFC 4122 v8 bit layout), bounded/sanitised summary (no
   transcripts; mentions already disabled by `Payload::wire`), owner lookup,
   capacity refusal (4096 episodes, 8192 receipts, 1024 windows).
4. **Runtime:** flush every 5 s from `notifications::runtime::run`; failure of
   flush never blocks dispatch. Delivery outcomes never call the policy (N4).
5. **Evidence:** `docs/implementation/F4.md`, live receipt via the opt-in test.
