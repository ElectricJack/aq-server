-- attempt.settle_closed_launch: who attested that a closed launch is over although its
-- closure does not prove that the harness never started, why and when. The closure
-- settlement records it in the transaction that settles the attempt. It is no
-- disposition: the task stays eligible for an ordinary replacement attempt.
CREATE TABLE closed_launch_attestations (
 attempt_id UUID PRIMARY KEY REFERENCES profiled_launch_closures(attempt_id),
 operation_id UUID NOT NULL UNIQUE CHECK(operation_id <> '00000000-0000-0000-0000-000000000000'),
 task_id UUID NOT NULL,
 project_id UUID NOT NULL,
 actor_id UUID NOT NULL REFERENCES users(id),
 actor TEXT NOT NULL CHECK(length(actor) BETWEEN 1 AND 320),
 reason TEXT NOT NULL CHECK(length(btrim(reason)) BETWEEN 1 AND 4096),
 evidence JSONB NOT NULL CHECK(jsonb_typeof(evidence)='object' AND octet_length(evidence::text) <= 4096),
 recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms > 0)
);

CREATE FUNCTION closed_launch_attestations_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'closed launch attestations are immutable';
END $$;

CREATE TRIGGER closed_launch_attestations_no_change BEFORE UPDATE OR DELETE ON closed_launch_attestations
 FOR EACH ROW EXECUTE FUNCTION closed_launch_attestations_immutable();
CREATE TRIGGER closed_launch_attestations_no_truncate BEFORE TRUNCATE ON closed_launch_attestations
 FOR EACH STATEMENT EXECUTE FUNCTION closed_launch_attestations_immutable();
UPDATE meta SET value='175' WHERE key='state_format_version';
