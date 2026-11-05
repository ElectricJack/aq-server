-- Human action admission/disposition only; domain owners retain effect authority.
CREATE TABLE dashboard_operations (
 operation_id UUID PRIMARY KEY CHECK(operation_id <> '00000000-0000-0000-0000-000000000000'::uuid),
 principal_id UUID NOT NULL REFERENCES users(id),
 kind TEXT NOT NULL CHECK(octet_length(kind) BETWEEN 3 AND 128),
 scope JSONB NOT NULL CHECK(jsonb_typeof(scope)='object'),
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^sha256:[0-9a-f]{64}$'),
 disposition TEXT NOT NULL CHECK(disposition IN ('pending','confirmed','conflict','rejected','unknown')),
 response JSONB,
 created_at TIMESTAMPTZ NOT NULL,
 updated_at TIMESTAMPTZ NOT NULL CHECK(updated_at >= created_at),
 retired_at TIMESTAMPTZ,
 CHECK((retired_at IS NULL)=(response IS NOT NULL)),
 CHECK(response IS NULL OR (jsonb_typeof(response)='object' AND octet_length(response::text)<=65536
   AND response->>'operation_id'=operation_id::text AND response->>'request_digest'=request_digest
   AND response->>'disposition'=disposition AND response->'scope'=scope)),
 CHECK(retired_at IS NULL OR (disposition IN ('confirmed','conflict','rejected') AND retired_at>=updated_at+interval '90 days'))
);
CREATE INDEX dashboard_operations_principal ON dashboard_operations(principal_id,created_at,operation_id);
CREATE INDEX dashboard_operations_retention ON dashboard_operations(updated_at) WHERE retired_at IS NULL AND disposition IN ('confirmed','conflict','rejected');
CREATE FUNCTION guard_dashboard_operation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'operation IDs cannot be removed'; END IF;
 IF ROW(NEW.operation_id,NEW.principal_id,NEW.kind,NEW.scope,NEW.request_digest,NEW.created_at)
 IS DISTINCT FROM ROW(OLD.operation_id,OLD.principal_id,OLD.kind,OLD.scope,OLD.request_digest,OLD.created_at) THEN
  RAISE EXCEPTION 'operation identity is immutable';
 END IF;
 IF OLD.retired_at IS NOT NULL AND NEW IS DISTINCT FROM OLD THEN RAISE EXCEPTION 'retired operation is immutable'; END IF;
 IF OLD.disposition IN ('confirmed','conflict','rejected') AND
   (NEW.disposition<>OLD.disposition OR NEW.updated_at<>OLD.updated_at OR (NEW.response IS DISTINCT FROM OLD.response AND NEW.retired_at IS NULL)) THEN
  RAISE EXCEPTION 'terminal receipt is immutable';
 END IF;
 IF NEW.updated_at<OLD.updated_at THEN RAISE EXCEPTION 'operation time cannot regress'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER dashboard_operation_guard BEFORE UPDATE OR DELETE ON dashboard_operations FOR EACH ROW EXECUTE FUNCTION guard_dashboard_operation();
CREATE TRIGGER dashboard_operation_truncate_guard BEFORE TRUNCATE ON dashboard_operations FOR EACH STATEMENT EXECUTE FUNCTION guard_dashboard_operation();
UPDATE meta SET value='42' WHERE key='state_format_version';
