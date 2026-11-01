# API-07 supervisor conversation foundation

This increment implements the durable conversation/history and human enqueue part
of API-07/UI-16. AQ-17 central execution remains absent. The UI can show accepted
human messages and explicitly unavailable runtime; it cannot claim delivery,
reasoning, a reply, or a running supervisor. No worker receives global mail.

The current AQ-11 owner supports only trusted task/session recipients. Its
`agent_launch_bindings` check cannot represent a global supervisor. AQ-19 does
not replace that check. Fabricating a task, worker or session would violate the
central-host design. Instead, state 35 introduces one durable global conversation
identity and retention counters. Its private logical recipient is separate from
the frozen Task/Session agent wire. Existing `mailbox_heads`, `mailbox_messages`
and `mailbox_operations` remain the only body, order and idempotency owners.

The message owner factors its private append primitive without changing legacy
request digests, send receipts, scope checks, or worker delivery. The new adapter
checks the current complete human Owner identity under lock, resolves the exact
singleton definition, locks the existing sender operation identity, and admits
one immutable mail record and receipt atomically. It never crosses a network or
provider boundary. A reused operation with different canonical request/context
or recipient conflicts. Response loss is recovered by pure receipt lookup or
repeating the same request and identities. Revision refusals and message-ID
collisions are retained not_queued receipts, so they cannot later turn into new
mail under the same operation identity.

All current Owners share the one global history. Viewers have no access, including
Viewers with session ACLs. Selected project scope changes neither recipient nor
authorization. This first adapter supports zero context refs and attachments;
nonempty values are rejected. Typed project/task/result/knowledge reference
admission is a later increment and must validate actual owner revisions. No
unvalidated URLs, raw artifacts or extra prose fields become privileged context.

History uses the existing indexed mailbox cursor, bounded to 16 messages and
256 KiB encoded response. Default reads the newest page, returned chronologically;
`before` retrieves older history and `after` observes newer messages. Both are
exclusive decimal cursors, never simultaneously accepted. Mailbox acceptance,
future host delivery and future agent receipt remain separate facts. Current
runtime state is unavailable with a null central binding; all delivery is
not_attempted and no assistant reply can be authored by this adapter.

Retention admits at most 4096 operations and 64 MiB reserved logical bytes. Replays
and reads remain available at capacity. No cleanup/downgrade policy is invented.
State 35 is additive after 34; old binaries refuse it. Rollback restores the
pre-upgrade database and matching binary. Frozen older migrations are unchanged.

AQ-17 must later own an authenticated central authority/session/generation,
resolved profile and expiring permit, plus exact dispatch associations and reply
authorship. Unknown handoff cannot automatically retarget a successor. The
logical conversation survives runtime replacement; this foundation does not
create runtime bindings, provision credentials, dispatch mail, or simulate replies.

Implementation gates: public contract/DDL review; current Owner and Viewer refusal;
actual persisted history/ordering and newest/older/newer paging; one message after
response loss/replay; changed payload and cross-domain operation conflicts;
retained revision/message-ID refusal; bounded admission and UTF-8; authorization
change during lock wait; existing AQ-11 wire regression; state 34 upgrade and old
reader refusal; no worker delivery allocation or provider effect. Browser evidence
must distinguish queued history from real supervisor reply qualification.

The [canonical state35 promotion](dashboard-canonical-state35-2026-11-01.md) now records the mounted foundation and exact 2b3d browser/visual evidence. Canonical history is empty; the one-enqueue positive case is disposable persisted human-message evidence. API-07/UI-16 remain in progress; central dispatch and live replies remain open.
