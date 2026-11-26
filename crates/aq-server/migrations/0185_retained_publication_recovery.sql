-- Exact-result publication dispatch fences and observation-only recovery receipts.
-- Historical host operations remain intact, including every remote rejection.
CREATE TABLE retained_publication_dispatches (
 report_id uuid PRIMARY KEY REFERENCES profiled_result_reports(report_id),
 operation_id uuid NOT NULL UNIQUE REFERENCES playbook_host_operations(operation_id),
 request jsonb NOT NULL,
 target jsonb NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now()
);
-- Existing operations may already have dispatched before this barrier existed.
-- Fence their first exact retained-result request, including a running original
-- whose reply was lost. Never resend it merely because it predates this schema.
INSERT INTO retained_publication_dispatches(report_id,operation_id,request,target)
 SELECT DISTINCT ON (r.report_id) r.report_id,o.operation_id,o.request,
        COALESCE(o.request->'publication_target','null'::jsonb)
 FROM playbook_host_operations o
 JOIN profiled_result_reports r ON o.request#>>'{result_ready,result_id}'=r.report_id::text
 JOIN execution_attempts a ON a.attempt_id=r.attempt_id AND o.subject_id=a.task_id::text
 JOIN delivery_pushed_results p ON p.report_id=r.report_id AND p.attempt_id=r.attempt_id
 WHERE o.command_name='branch.publish' AND o.request->>'result_head'=p.head
 ORDER BY r.report_id,o.created_at_ms,o.operation_id;

-- Frozen before operation.reconcile: a lost reply must reuse its original generation.
CREATE TABLE retained_publication_recovery_requests (
 operation_id uuid PRIMARY KEY,
 publication_operation_id uuid NOT NULL REFERENCES playbook_host_operations(operation_id),
 report_id uuid NOT NULL REFERENCES profiled_result_reports(report_id),
 request jsonb NOT NULL,
 publisher jsonb NOT NULL CHECK (octet_length(publisher::text) <= 8192)
);
CREATE TRIGGER retained_publication_recovery_request_immutable BEFORE UPDATE OR DELETE ON retained_publication_recovery_requests
 FOR EACH ROW EXECUTE FUNCTION delivery_immutable();
CREATE TABLE retained_publication_recoveries (
 operation_id uuid PRIMARY KEY REFERENCES retained_publication_recovery_requests(operation_id),
 publication_operation_id uuid NOT NULL REFERENCES playbook_host_operations(operation_id),
 report_id uuid NOT NULL REFERENCES profiled_result_reports(report_id),
 request jsonb NOT NULL,
 evidence jsonb NOT NULL CHECK (octet_length(evidence::text) <= 16384),
 recorded_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX retained_publication_recovery_report ON retained_publication_recoveries(report_id);
CREATE TRIGGER retained_publication_dispatch_immutable BEFORE UPDATE OR DELETE ON retained_publication_dispatches
 FOR EACH ROW EXECUTE FUNCTION delivery_immutable();
CREATE TRIGGER retained_publication_recovery_immutable BEFORE UPDATE OR DELETE ON retained_publication_recoveries
 FOR EACH ROW EXECUTE FUNCTION delivery_immutable();
UPDATE meta SET value='185' WHERE key='state_format_version';
