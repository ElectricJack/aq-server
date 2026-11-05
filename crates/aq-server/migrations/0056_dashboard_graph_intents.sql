-- Exact native intent for typed Graph actions. A lookup replays this body/key,
-- never reconstructing an operation from changed live node metadata.
CREATE TABLE dashboard_graph_intents (
 operation_id UUID PRIMARY KEY REFERENCES dashboard_operations(operation_id),
 project_id UUID NOT NULL,
 project_revision BIGINT NOT NULL CHECK(project_revision > 0),
 project_binding JSONB NOT NULL,
 workspace_id UUID NOT NULL,
 service_identity JSONB NOT NULL,
 idempotency_key TEXT NOT NULL CHECK(idempotency_key='aq-dashboard-action:' || operation_id::text),
 body BYTEA NOT NULL CHECK(octet_length(body) BETWEEN 1 AND 1048576)
);
CREATE FUNCTION guard_dashboard_graph_intent() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'Graph dispatch intent is immutable';
END $$;
CREATE TRIGGER dashboard_graph_intent_guard BEFORE UPDATE OR DELETE ON dashboard_graph_intents FOR EACH ROW EXECUTE FUNCTION guard_dashboard_graph_intent();
CREATE TRIGGER dashboard_graph_intent_truncate_guard BEFORE TRUNCATE ON dashboard_graph_intents FOR EACH STATEMENT EXECUTE FUNCTION guard_dashboard_graph_intent();
UPDATE meta SET value='56' WHERE key='state_format_version';
