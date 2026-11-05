-- Supplemental product projections. Graph remains the sole task lifecycle owner.
CREATE TABLE result_task_policies (
 project_id uuid NOT NULL, task_id uuid NOT NULL, revision bigint NOT NULL CHECK(revision>0),
 requirements jsonb NOT NULL CHECK(jsonb_typeof(requirements)='array' AND jsonb_array_length(requirements)<=64 AND octet_length(requirements::text)<=65536),
 PRIMARY KEY(project_id,task_id,revision)
);
CREATE TABLE result_aggregates (
 result_id uuid PRIMARY KEY REFERENCES profiled_result_reports(report_id),
 version bigint NOT NULL DEFAULT 1 CHECK(version>0)
);
CREATE TABLE result_revision_pins (
 report_id uuid PRIMARY KEY REFERENCES profiled_result_reports(report_id),
 result_id uuid NOT NULL REFERENCES result_aggregates(result_id),
 report_revision text NOT NULL CHECK(report_revision ~ '^[0-9a-f]{64}$'),
 project_id uuid NOT NULL, task_id uuid NOT NULL, submitted_at_ms bigint NOT NULL,
 capture_policy jsonb NOT NULL,
 UNIQUE(report_id,report_revision)
);
CREATE INDEX result_feed ON result_revision_pins(submitted_at_ms DESC,report_id DESC);
CREATE TABLE result_assessments (
 report_id uuid PRIMARY KEY REFERENCES result_revision_pins(report_id),
 version bigint NOT NULL DEFAULT 1 CHECK(version>0), observed_state jsonb
);
CREATE FUNCTION pin_result_feedback_revision(new_id uuid) RETURNS void LANGUAGE plpgsql AS $$
DECLARE spec jsonb; root uuid; policy jsonb; NEW profiled_result_reports%ROWTYPE;
BEGIN
 SELECT * INTO STRICT NEW FROM profiled_result_reports WHERE report_id=new_id;
 SELECT execution_spec INTO STRICT spec FROM profiled_launches WHERE attempt_id=NEW.attempt_id;
 root:=NEW.report_id;
 IF NEW.supersedes IS NOT NULL THEN
  SELECT result_id INTO STRICT root FROM result_revision_pins WHERE report_id=NEW.supersedes;
  UPDATE result_aggregates SET version=version+1 WHERE result_id=root;
 ELSE
  INSERT INTO result_aggregates(result_id) VALUES(root);
 END IF;
 SELECT jsonb_build_object('revision',revision::text,'requirements',requirements) INTO policy FROM result_task_policies
 WHERE project_id=(spec#>>'{lifecycle,project_id}')::uuid AND task_id=(spec#>>'{task,task_id}')::uuid ORDER BY revision DESC LIMIT 1;
 INSERT INTO result_revision_pins VALUES(NEW.report_id,root,NEW.report_digest,(spec#>>'{lifecycle,project_id}')::uuid,(spec#>>'{task,task_id}')::uuid,(convert_from(NEW.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint,COALESCE(policy,'{"revision":"0","requirements":[]}'::jsonb));
 INSERT INTO result_assessments(report_id) VALUES(NEW.report_id);
 RETURN;
END $$;
-- Backfill in ancestry order; historical reports explicitly have no capture policy.
WITH RECURSIVE ancestry AS (
 SELECT report_id,report_id AS root,0 AS depth FROM profiled_result_reports WHERE supersedes IS NULL
 UNION ALL SELECT r.report_id,a.root,a.depth+1 FROM profiled_result_reports r JOIN ancestry a ON r.supersedes=a.report_id
)
INSERT INTO result_aggregates(result_id,version) SELECT root,count(*) FROM ancestry GROUP BY root;
WITH RECURSIVE ancestry AS (
 SELECT report_id,report_id AS root FROM profiled_result_reports WHERE supersedes IS NULL
 UNION ALL SELECT r.report_id,a.root FROM profiled_result_reports r JOIN ancestry a ON r.supersedes=a.report_id
)
INSERT INTO result_revision_pins
 SELECT r.report_id,a.root,r.report_digest,(l.execution_spec#>>'{lifecycle,project_id}')::uuid,(l.execution_spec#>>'{task,task_id}')::uuid,(convert_from(r.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint,'{"revision":"0","requirements":[]}'::jsonb
 FROM ancestry a JOIN profiled_result_reports r USING(report_id) JOIN profiled_launches l USING(attempt_id);
INSERT INTO result_assessments(report_id) SELECT report_id FROM result_revision_pins;
-- The report owner calls pin_result_feedback_revision in its insertion transaction.

CREATE TABLE result_threads (
 id uuid PRIMARY KEY, report_id uuid NOT NULL, report_revision text NOT NULL,
 version bigint NOT NULL DEFAULT 1 CHECK(version>0), state text NOT NULL DEFAULT 'open' CHECK(state IN ('open','resolved')),
 anchor jsonb NOT NULL, resolved_by uuid REFERENCES users(id), reason text,
 follow_up_report uuid, follow_up_revision text,
 FOREIGN KEY(report_id,report_revision) REFERENCES result_revision_pins(report_id,report_revision),
 FOREIGN KEY(follow_up_report,follow_up_revision) REFERENCES result_revision_pins(report_id,report_revision),
 CHECK((state='resolved')=(resolved_by IS NOT NULL AND reason IS NOT NULL AND follow_up_report IS NOT NULL AND follow_up_revision IS NOT NULL))
);
CREATE TABLE result_comments (
 id uuid PRIMARY KEY, thread_id uuid NOT NULL REFERENCES result_threads(id),
 revision bigint NOT NULL DEFAULT 1 CHECK(revision BETWEEN 1 AND 16), created_at_ms bigint NOT NULL,
 position bigint GENERATED ALWAYS AS IDENTITY UNIQUE
);
CREATE TABLE result_comment_revisions (
 comment_id uuid NOT NULL REFERENCES result_comments(id), revision bigint NOT NULL CHECK(revision>0),
 author_id uuid NOT NULL REFERENCES users(id), body text NOT NULL CHECK(octet_length(body) BETWEEN 1 AND 16384),
 recorded_at_ms bigint NOT NULL, PRIMARY KEY(comment_id,revision)
);
CREATE TABLE result_follows (
 result_id uuid NOT NULL REFERENCES result_aggregates(result_id), actor_id uuid NOT NULL REFERENCES users(id),
 following boolean NOT NULL, PRIMARY KEY(result_id,actor_id)
);
CREATE TABLE result_thread_follows (
 thread_id uuid NOT NULL REFERENCES result_threads(id), actor_id uuid NOT NULL REFERENCES users(id),
 following boolean NOT NULL, PRIMARY KEY(thread_id,actor_id)
);
CREATE TABLE result_feedback_intents (
 operation_id uuid PRIMARY KEY, actor_id uuid NOT NULL REFERENCES users(id),
 report_id uuid NOT NULL, report_revision text NOT NULL, request jsonb NOT NULL,
 FOREIGN KEY(report_id,report_revision) REFERENCES result_revision_pins(report_id,report_revision)
);
CREATE TABLE result_feedback_links (
 operation_id uuid PRIMARY KEY, report_id uuid NOT NULL, report_revision text NOT NULL,
 task_id uuid NOT NULL, project_id uuid NOT NULL, origin jsonb NOT NULL,
 FOREIGN KEY(report_id,report_revision) REFERENCES result_revision_pins(report_id,report_revision)
);
CREATE TABLE result_evidence_waivers (
 operation_id uuid PRIMARY KEY, report_id uuid NOT NULL, report_revision text NOT NULL,
 requirement_id text NOT NULL, policy_revision text NOT NULL, actor_id uuid NOT NULL REFERENCES users(id),
 reason text NOT NULL CHECK(octet_length(reason) BETWEEN 1 AND 1024), recorded_at_ms bigint NOT NULL,
 FOREIGN KEY(report_id,report_revision) REFERENCES result_revision_pins(report_id,report_revision),
 UNIQUE(report_id,requirement_id,policy_revision)
);
CREATE TABLE result_feedback_effects (
 operation_id uuid PRIMARY KEY, actor_id uuid NOT NULL REFERENCES users(id),
 report_id uuid NOT NULL REFERENCES result_revision_pins(report_id), outcome jsonb NOT NULL
);
CREATE FUNCTION result_feedback_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'retained result feedback history is immutable'; END $$;
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['result_task_policies','result_revision_pins','result_comment_revisions','result_feedback_intents','result_feedback_links','result_evidence_waivers','result_feedback_effects'] LOOP
  EXECUTE format('CREATE TRIGGER immutable_row BEFORE UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION result_feedback_immutable()',t);
  EXECUTE format('CREATE TRIGGER immutable_all BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION result_feedback_immutable()',t);
 END LOOP;
END $$;
UPDATE meta SET value='44' WHERE key='state_format_version';

-- Policies are administrative task-policy records, never client report claims.
-- Reject malformed or unbounded requirements before any report can snapshot them.
CREATE FUNCTION validate_result_task_policy() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r jsonb; ids text[] := ARRAY[]::text[]; s jsonb;
BEGIN
 FOR r IN SELECT * FROM jsonb_array_elements(NEW.requirements) LOOP
  IF jsonb_typeof(r)<>'object' OR (SELECT count(*) FROM jsonb_object_keys(r))<>11
   OR NOT (r ?& ARRAY['id','policy_revision','kind','scenario','steps','viewport','environment','count','required','applicability','gate_binding'])
   OR jsonb_typeof(r->'id')<>'string' OR length(btrim(r->>'id')) NOT BETWEEN 1 AND 128 OR (r->>'id')=ANY(ids)
   OR r->>'policy_revision'<>NEW.revision::text OR jsonb_typeof(r->'policy_revision')<>'string'
   OR r->>'kind'<>'image' OR jsonb_typeof(r->'steps')<>'array' OR jsonb_array_length(r->'steps')>64
   OR jsonb_typeof(r->'count')<>'number' OR (r->>'count')!~'^[1-8]$'
   OR jsonb_typeof(r->'required')<>'boolean'
   OR jsonb_typeof(r->'viewport')<>'object' OR (SELECT count(*) FROM jsonb_object_keys(r->'viewport'))<>3
   OR NOT ((r->'viewport') ?& ARRAY['width_css','height_css','dpr_milli'])
   OR EXISTS(SELECT 1 FROM unnest(ARRAY['width_css','height_css','dpr_milli']) k WHERE jsonb_typeof(r->'viewport'->k)<>'number')
   OR (r#>>'{viewport,width_css}')!~'^[1-9][0-9]{0,4}$' OR (r#>>'{viewport,height_css}')!~'^[1-9][0-9]{0,4}$'
   OR (r#>>'{viewport,dpr_milli}')!~'^[1-9][0-9]{0,4}$'
   OR (r#>>'{viewport,width_css}')::bigint>4096 OR (r#>>'{viewport,height_css}')::bigint>4096
   OR (r#>>'{viewport,dpr_milli}')::bigint NOT BETWEEN 100 AND 8000
   OR octet_length(r->>'scenario')>128 OR octet_length(r->>'environment')>128
   OR EXISTS(SELECT 1 FROM unnest(ARRAY['scenario','environment','applicability','gate_binding']) k WHERE jsonb_typeof(r->k)<>'string' OR length(btrim(r->>k)) NOT BETWEEN 1 AND 2048)
  THEN RAISE EXCEPTION 'invalid capture requirement'; END IF;
  FOR s IN SELECT * FROM jsonb_array_elements(r->'steps') LOOP
   IF jsonb_typeof(s)<>'string' OR length(btrim(s#>>'{}')) NOT BETWEEN 1 AND 2048 THEN RAISE EXCEPTION 'invalid capture step'; END IF;
  END LOOP;
  ids:=array_append(ids,r->>'id');
 END LOOP;
 RETURN NEW;
END $$;
CREATE TRIGGER validate_policy BEFORE INSERT ON result_task_policies FOR EACH ROW EXECUTE FUNCTION validate_result_task_policy();
CREATE INDEX result_thread_report ON result_threads(report_id,state);
CREATE INDEX result_comment_thread ON result_comments(thread_id,position);
CREATE INDEX result_follow_actor ON result_follows(actor_id,result_id) WHERE following;
CREATE INDEX result_thread_follow_actor ON result_thread_follows(actor_id,thread_id) WHERE following;
