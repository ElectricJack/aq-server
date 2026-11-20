-- Run as the disposable Host10 database owner. Supply reader (an existing
-- dedicated login), project_id, repository_id, binding_generation, and
-- enrolled_after_ms with psql -v. The view owner needs to read the base table.
BEGIN;
CREATE SCHEMA aq_graph_deploy_export;
CREATE VIEW aq_graph_deploy_export.merged WITH (security_barrier = true) AS
    SELECT operation_id, pull_request, merge_sha, observed_at_ms
    FROM public.delivery_merge_evidence
    WHERE project_id = :'project_id'::uuid
      AND target #>> '{git,repository_id}' = :'repository_id'::uuid::text
      AND target #>> '{git,binding_generation}' = :'binding_generation'::bigint::text
      AND lower(target #>> '{github,owner}') = 'electricjack'
      AND lower(target #>> '{github,name}') = 'aq-graph'
      AND target ->> 'default_branch' = 'main'
      AND target_ref = 'refs/heads/main'
      AND observed_at_ms >= :'enrolled_after_ms'::bigint;
REVOKE ALL ON SCHEMA aq_graph_deploy_export FROM PUBLIC;
REVOKE ALL ON aq_graph_deploy_export.merged FROM PUBLIC;
GRANT USAGE ON SCHEMA aq_graph_deploy_export TO :"reader";
GRANT SELECT ON aq_graph_deploy_export.merged TO :"reader";
COMMIT;
