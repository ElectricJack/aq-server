-- G11 Decisions view (X16, X18, X19). Additive only: human_decision_requests
-- is pinned by the settlement39 catalog attestation, so submission time lives
-- beside it. Decisions admitted before this format have no submission row.
CREATE TABLE human_decision_submissions (
 decision_id uuid PRIMARY KEY REFERENCES human_decision_requests(decision_id),
 submitted_at_ms bigint NOT NULL CHECK (submitted_at_ms >= 0)
);
-- Defer is a per-owner, non-final snooze. It never answers, cancels or
-- re-digests the decision; the decision still expires at its sealed time.
CREATE TABLE human_decision_deferrals (
 decision_id uuid NOT NULL REFERENCES human_decision_requests(decision_id),
 actor_id uuid NOT NULL REFERENCES users(id),
 operation_id uuid NOT NULL,
 deferred_until_ms bigint NOT NULL CHECK (deferred_until_ms > 0),
 deferred_at_ms bigint NOT NULL CHECK (deferred_at_ms >= 0),
 PRIMARY KEY(decision_id, actor_id)
);
-- First time an owner opened one pinned evidence reference of a decision.
CREATE TABLE human_decision_evidence_views (
 decision_id uuid NOT NULL REFERENCES human_decision_requests(decision_id),
 actor_id uuid NOT NULL REFERENCES users(id),
 evidence_id text NOT NULL CHECK (evidence_id ~ '^[a-z_]{1,32}$'),
 ref_digest text NOT NULL CHECK (octet_length(ref_digest) BETWEEN 1 AND 128),
 viewed_at_ms bigint NOT NULL CHECK (viewed_at_ms >= 0),
 PRIMARY KEY(decision_id, actor_id, evidence_id)
);
UPDATE meta SET value='73' WHERE key='state_format_version';
