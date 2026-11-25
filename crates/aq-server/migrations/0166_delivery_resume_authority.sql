-- A renewed grant can authorize one already-admitted delivery decision.
-- Its original effect identity and authority remain immutable.
CREATE TABLE playbook_delivery_resumptions (
 approval_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 operation_id UUID NOT NULL REFERENCES playbook_effects(operation_id) ON DELETE CASCADE,
 grant_id UUID NOT NULL REFERENCES playbook_grants(grant_id),
 grant_revision BIGINT NOT NULL CHECK (grant_revision > 0),
 approved_at_ms BIGINT NOT NULL CHECK (approved_at_ms > 0),
 UNIQUE(operation_id, grant_id)
);
CREATE INDEX playbook_delivery_resume_latest
 ON playbook_delivery_resumptions(operation_id, approval_id DESC);

CREATE FUNCTION playbook_delivery_resume_current(p_operation UUID, p_now_ms BIGINT)
RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT COALESCE((SELECT renewed.revision=approval.grant_revision
       AND renewed.revoked_at_ms IS NULL AND renewed.expires_at_ms>p_now_ms
       AND renewed.owner_user_id=original.owner_user_id AND renewed.scope=original.scope
 FROM playbook_effects e
 JOIN playbook_grants original ON original.grant_id=e.grant_id
  AND original.namespace_id=e.namespace_id AND original.generation=e.generation
 LEFT JOIN LATERAL (
  SELECT r.grant_id,r.grant_revision FROM playbook_delivery_resumptions r
  WHERE r.operation_id=e.operation_id ORDER BY r.approval_id DESC LIMIT 1
 ) approval ON true
 LEFT JOIN playbook_grants renewed ON renewed.grant_id=approval.grant_id
  AND renewed.namespace_id=e.namespace_id AND renewed.generation=e.generation
 WHERE e.operation_id=p_operation AND original.revision=e.grant_revision
   AND e.command_name IN ('work.complete','work.return')), false)
$$;

UPDATE meta SET value='166' WHERE key='state_format_version';
