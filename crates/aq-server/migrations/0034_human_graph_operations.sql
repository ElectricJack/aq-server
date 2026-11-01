-- Human graph intent journal only. Graph remains the task-record authority.
CREATE TABLE human_graph_operations (
 operation_id UUID PRIMARY KEY,
 actor_id UUID NOT NULL REFERENCES users(id),
 project_id UUID NOT NULL REFERENCES project_execution_bindings(project_id),
 project_revision BIGINT NOT NULL CHECK(project_revision>0),
 project_binding JSONB NOT NULL CHECK(jsonb_typeof(project_binding)='object'),
 workspace_id UUID NOT NULL,
 task_id UUID NOT NULL CHECK(task_id=operation_id),
 request_bytes BYTEA NOT NULL CHECK(octet_length(request_bytes) BETWEEN 1 AND 65536),
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^sha256:[0-9a-f]{64}$'),
 payload_bytes BYTEA NOT NULL CHECK(octet_length(payload_bytes) BETWEEN 1 AND 131072),
 payload_digest TEXT NOT NULL CHECK(payload_digest ~ '^sha256:[0-9a-f]{64}$'),
 service_identity JSONB NOT NULL CHECK(jsonb_typeof(service_identity)='object'),
 status JSONB NOT NULL CHECK(jsonb_typeof(status)='object' AND status ? 'state' AND jsonb_typeof(status->'state')='string' AND status->>'state' IN ('pending','unknown','confirmed','conflict','rejected')),
 raw_receipt JSONB,
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>0),
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>created_at_ms AND expires_at_ms-created_at_ms<=300000),
 dispatch_count INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_count BETWEEN 0 AND 8),
 lease_id UUID,
 lease_expires_at_ms BIGINT,
 stop_reason TEXT,
 retained_bytes BIGINT NOT NULL CHECK(retained_bytes>0 AND retained_bytes<=262144),
 CHECK((lease_id IS NULL)=(lease_expires_at_ms IS NULL)),
 CHECK((status->>'state'='confirmed')=(raw_receipt IS NOT NULL)),
 CHECK(raw_receipt IS NULL OR jsonb_typeof(raw_receipt)='object'),
 CHECK(operation_id <> '00000000-0000-0000-0000-000000000000'::uuid AND workspace_id <> '00000000-0000-0000-0000-000000000000'::uuid)
);
CREATE INDEX human_graph_operation_actor ON human_graph_operations(actor_id,project_id,created_at_ms,operation_id);
CREATE FUNCTION guard_human_graph_operation_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'human Graph operation history cannot be deleted'; END IF;
 IF ROW(NEW.operation_id,NEW.actor_id,NEW.project_id,NEW.project_revision,NEW.project_binding,NEW.workspace_id,NEW.task_id,NEW.request_bytes,NEW.request_digest,NEW.payload_bytes,NEW.payload_digest,NEW.service_identity,NEW.created_at_ms,NEW.expires_at_ms,NEW.retained_bytes)
 IS DISTINCT FROM ROW(OLD.operation_id,OLD.actor_id,OLD.project_id,OLD.project_revision,OLD.project_binding,OLD.workspace_id,OLD.task_id,OLD.request_bytes,OLD.request_digest,OLD.payload_bytes,OLD.payload_digest,OLD.service_identity,OLD.created_at_ms,OLD.expires_at_ms,OLD.retained_bytes) THEN
  RAISE EXCEPTION 'human Graph operation identity is immutable';
 END IF;
 IF OLD.status->>'state' IN ('confirmed','conflict','rejected') AND NEW IS DISTINCT FROM OLD THEN
  RAISE EXCEPTION 'human Graph terminal operation is immutable';
 END IF;
 IF NEW.dispatch_count<OLD.dispatch_count THEN RAISE EXCEPTION 'human Graph dispatch history cannot decrease'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER human_graph_operation_identity_guard BEFORE UPDATE ON human_graph_operations FOR EACH ROW EXECUTE FUNCTION guard_human_graph_operation_identity();
CREATE TRIGGER human_graph_operation_delete_guard BEFORE DELETE ON human_graph_operations FOR EACH ROW EXECUTE FUNCTION guard_human_graph_operation_identity();
CREATE TRIGGER human_graph_operation_truncate_guard BEFORE TRUNCATE ON human_graph_operations FOR EACH STATEMENT EXECUTE FUNCTION guard_human_graph_operation_identity();
UPDATE meta SET value='34' WHERE key='state_format_version';
