-- H3 stall ladder. One episode per profiled attempt. Every ladder action has a
-- stable ID, the episode revision it was decided at, and an effect receipt;
-- an unreceipted action is re-run (idempotently) after a restart. Replacement
-- ceilings are lineage-wide (lineage = task) and never reset by progress.
CREATE TABLE stall_episodes (
 episode_id UUID PRIMARY KEY,
 attempt_id UUID NOT NULL UNIQUE REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
 lineage_id UUID NOT NULL,
 policy_version INTEGER NOT NULL CHECK(policy_version>0),
 revision BIGINT NOT NULL CHECK(revision>=0),
 rung TEXT NOT NULL CHECK(rung IN ('observing','suspected','nudging','notified','stopping','replacing','replaced','quarantined')),
 state JSONB NOT NULL CHECK(octet_length(state::text)<=8192),
 updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms>=0)
);
CREATE INDEX stall_episodes_lineage ON stall_episodes(lineage_id);
CREATE TABLE stall_actions (
 action_id UUID PRIMARY KEY,
 episode_id UUID NOT NULL REFERENCES stall_episodes(episode_id) ON DELETE CASCADE,
 kind TEXT NOT NULL CHECK(kind IN ('monitoring_incident','nudge','notify','stop','replace','quarantine')),
 ordinal INTEGER NOT NULL CHECK(ordinal>0),
 expected_revision BIGINT NOT NULL CHECK(expected_revision>=0),
 intent JSONB NOT NULL CHECK(octet_length(intent::text)<=1024),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>=0),
 receipt JSONB CHECK(octet_length(receipt::text)<=2048),
 receipted_at_ms BIGINT CHECK(receipted_at_ms>=0),
 -- Nudges only: attempted and deferred at send; delivered and unknown follow
 -- the F3 delivery record.
 delivery TEXT CHECK(delivery IN ('attempted','deferred','delivered','unknown')),
 UNIQUE(episode_id,kind,ordinal),
 CHECK((receipt IS NULL)=(receipted_at_ms IS NULL)),
 CHECK(delivery IS NULL OR kind='nudge')
);
-- At most one in-flight action per episode.
CREATE UNIQUE INDEX stall_actions_pending ON stall_actions(episode_id) WHERE receipt IS NULL;
CREATE TABLE stall_lineage_budgets (
 lineage_id UUID PRIMARY KEY,
 replacements INTEGER NOT NULL CHECK(replacements>=0),
 updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms>=0)
);
-- F3 messages that are stall nudges. Inbox-only nudges are never handed to a
-- harness, and no stall nudge is handed over while the attempt's terminal
-- input is unknown or a human controller holds the pane.
CREATE TABLE stall_nudges (
 message_id UUID PRIMARY KEY REFERENCES mailbox_messages(message_id) ON DELETE CASCADE,
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
 action_id UUID REFERENCES stall_actions(action_id) ON DELETE SET NULL,
 inbox_only BOOLEAN NOT NULL,
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>=0)
);
CREATE INDEX stall_nudges_attempt ON stall_nudges(attempt_id);
-- Rung 5: scoped to one session and its task lineage, never the worker.
-- B4 refuses replacement for a quarantined lineage.
CREATE TABLE stall_quarantines (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
 lineage_id UUID NOT NULL,
 reason TEXT NOT NULL CHECK(octet_length(reason) BETWEEN 1 AND 128),
 escalation TEXT NOT NULL CHECK(octet_length(escalation) BETWEEN 1 AND 64),
 held JSONB NOT NULL CHECK(octet_length(held::text)<=4096),
 action_id UUID NOT NULL,
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>=0)
);
CREATE INDEX stall_quarantines_lineage ON stall_quarantines(lineage_id);
ALTER TABLE notification_episodes DROP CONSTRAINT notification_episodes_kind_check;
ALTER TABLE notification_episodes ADD CONSTRAINT notification_episodes_kind_check CHECK(kind IN ('task.failed_or_blocked','decision.needed','question.asked','credential.invalid','milestone.gate_ready','worker.offline','storage.pressure','sandbox.preserved','session.stalled'));
UPDATE meta SET value='81' WHERE key='state_format_version';
