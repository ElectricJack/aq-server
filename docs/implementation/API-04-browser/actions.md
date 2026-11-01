# Disposable Decisions action browser qualification

Separate from the read-only proof, the same actual SQL/review fixture and unchanged 28-asset production bundle passed browser action qualification in **2.88 seconds**. All three target reviews belonged to a newly created isolated test database. No user record, provider, external destination or source workspace was modified.

The browser double-clicked Approve and Request changes on two reviews. Each produced exactly one resolve POST and the exact recorded actor/operation/choice receipt. For the third review, an explicit loopback HTTP fault proxy forwarded the real answer to the protected owner, validated its committed receipt, then truncated the actual HTTP reply after headers. The UI displayed delivery as unknown and preserved the frozen answer. Clicking Retry same answer issued identical request bytes and recovered the exact original receipt. Four resolve POSTs covered three decisions. Subject/source/report pins remained unchanged; the UI showed recorded decision state and did not claim task completion or merge. See [machine-readable assertions](actions.json).

This is an actual browser → HTTP → review owner path. No browser response interception, mocked decision handler or product-code change was used. The test proxy rebases Origin to the explicit source router for its local fault-injection case; the first two direct cases use the ordinary browser Origin and actual CSRF middleware. This does not qualify arbitrary proxy deployment.

An initial pre-header socket-only drop did not expose the intended unknown UI state: a successful recorded answer was observed instead. That invocation failed its uncertainty assertion and remains at `/tmp/aq-decisions-actions-socket-drop-first.log`. The corrected test truncates after actual response headers to make transport uncertainty observable without relaxing assertions or deadlines.

Reproduce only through the disposable fixture, using its automatically supplied ephemeral origin and exact generated decision ID:

```sh
DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres \
AQ_TEST_REVIEW_BROWSER_SCRIPT=/absolute/dashboard/scripts/probe-disposable-decision-actions.mjs \
AQ_TEST_REVIEW_DASHBOARD_DIR=/absolute/dashboard/dist \
AQ_TEST_DISPOSABLE_REVIEW_ACTIONS=1 \
cargo test -p aq-server --test profiled_results \
  review_inbox::authorized_review_inbox_pages_actual_owner_records_and_binds_cursors \
  -- --exact --nocapture --test-threads=1
```

The script refuses a missing disposable acknowledgement, non-loopback origin, missing exact review ID, unexpected fixture cardinality or ineligible actor. Screenshot `/tmp/aq-decisions-action-evidence/recorded-disposable-answer.png` was visually reviewed. Bundle manifest SHA256 remains `85fa3dbfb790467dcaca3be6ce6ee7f448d9b2b39603f066df3b3811afbde2a8`; no new server or dashboard build was needed for this followup.
