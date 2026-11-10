# Manual agent intelligence selection

Owner request: launch Codex as well as Claude and select a saved intelligence profile. Continue afterward with supervisor backend/frontend completion.

Ruling (my call): compact inline Provider and Intelligence profile selectors; no assignment/modal. Resolve the exact saved profile version and candidate on the server into immutable session settings. Manual owner choices may use any active saved profile without changing native project routing or qualifications. Reject stale/deleted choices and unsupported worker providers. Existing sessions retain their launch snapshot.

Implementation: extend original-worker reports with installed manual provider capabilities (migration 108); expose saved candidates resolved against the existing reasoning metadata. Send only profile/version/candidate identity from the browser. Start bare Codex in the same fixed sandbox/tmux transport using isolated CODEX_HOME and privately delivered ChatGPT subscription material. No API keys, host config copying, or credential logging. Preserve Claude, direct terminal streaming, source retention and exact uncertain-launch retry.

Validation: existing server real-SQL test for exact selection, stale/refused/unsupported and idempotent retry; existing worker binary tests for provider settings and no-prompt launch scripts; focused UI tests for choices and exact retry. Check installed CLI flags, desktop/phone live launch, actual tiny Codex reply, persistent reconnect and explicit QA stop. Stage all changes, then deploy only isolated aq-ms1 while idle; protect local worker/Supabase/v1 and owner sessions. Keep draft review branches unmerged. Record evidence and limitations in main supervision log/retrospective.
