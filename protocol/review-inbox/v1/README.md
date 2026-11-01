# review-inbox-1

Human-only discovery contract generated from `aq_server::reviews::InboxPage`.

Regenerate with `cargo run -p aq-server --example review_inbox_contract --locked --offline > protocol/review-inbox/v1/schema.json`.

The configured human router mounts `GET /api/reviews/decisions`. Query parameters, scope/cursor semantics and exact-ID evidence behavior are documented in [API-04](../../../docs/implementation/API-04-review-inbox.md). The existing review decision request/receipt protocol remains owned by its versioned review contract.
