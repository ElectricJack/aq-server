# Configured Decisions browser qualification

The actual retained-result/review fixture served its protected review owner and the verified production dashboard bundle through the combined human admin builder. The browser read three persisted reviews, exact retained acceptance text and source objects at 1440px desktop and 390px phone. No browser response interception or mutation was used. Desktop/mobile screenshots were visually reviewed; inbox, evidence and context panes rendered, with no page errors or document horizontal overflow.

The complete actual SQL/router/browser test passed in **2.75 seconds**. Strict library plus `profiled_results` Clippy with all features passed in **29.82 seconds**. See [probe result](result.json).

Exact bundle manifest SHA256: `85fa3dbfb790467dcaca3be6ce6ee7f448d9b2b39603f066df3b3811afbde2a8` (28 assets; runtime code paired with dashboard `6a309ea`). Screenshots remain local under `/tmp/aq-decisions-browser-evidence/decisions-{desktop,mobile}.png`.

Reproduce the optional qualification after building `profiled_results`:

```sh
DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres \
AQ_TEST_REVIEW_BROWSER_SCRIPT=/absolute/dashboard/scripts/probe-live-decisions.mjs \
AQ_TEST_REVIEW_DASHBOARD_DIR=/absolute/dashboard/dist \
cargo test -p aq-server --test profiled_results \
  review_inbox::authorized_review_inbox_pages_actual_owner_records_and_binds_cursors \
  -- --exact --nocapture --test-threads=1
```

Without these explicit browser inputs, the ordinary owner test requires no sibling dashboard, Node or browser. The listener uses the same identity/capability/CSRF/maintenance construction as the configured human admin router. A labelled local test fixture supplies the source-retention peer and native result inputs; this does not qualify actual Git transfer, a real model, production data, decision actions or full mockup fidelity. Full artifact media/discussion and richer task titles remain separate owners.

A separate [disposable browser action qualification](actions.md) now covers actual approve/reject double clicks and exact-receipt recovery after a committed response is truncated. The read-only proof above remains separately scoped.
