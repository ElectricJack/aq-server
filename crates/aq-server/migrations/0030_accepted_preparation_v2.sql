-- Separate immutable phase-v2 provenance. No legacy plan is rewritten.
CREATE TABLE profiled_v2_proposals(
 attempt_id UUID PRIMARY KEY REFERENCES profiled_launches(attempt_id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 boot_id UUID NOT NULL,
 policy JSONB NOT NULL CHECK(octet_length(policy::text)<=4096),
 proposal JSONB NOT NULL CHECK(octet_length(proposal::text)<=262144),
 proposal_digest TEXT NOT NULL CHECK(proposal_digest ~ '^[0-9a-f]{64}$'),
 published_at_ms BIGINT NOT NULL CHECK(published_at_ms>0)
);
CREATE INDEX profiled_v2_proposals_node ON profiled_v2_proposals(node_id);
CREATE TABLE profiled_v2_accepts(
 attempt_id UUID PRIMARY KEY REFERENCES profiled_v2_proposals(attempt_id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 operation_id UUID NOT NULL,
 request JSONB NOT NULL CHECK(octet_length(request::text)<=262144),
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 receipt JSONB NOT NULL CHECK(octet_length(receipt::text)<=262144),
 receipt_digest TEXT NOT NULL CHECK(receipt_digest ~ '^[0-9a-f]{64}$'),
 lease_id UUID NOT NULL UNIQUE REFERENCES admission_material_leases(lease_id),
 issued_at_ms BIGINT NOT NULL CHECK(issued_at_ms>0),
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>issued_at_ms),
 UNIQUE(node_id,operation_id)
);
CREATE INDEX profiled_v2_accepts_node ON profiled_v2_accepts(node_id);
CREATE FUNCTION refuse_profiled_v2_history_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'immutable accepted preparation v2 history'; END $$;
CREATE TRIGGER profiled_v2_proposal_immutable BEFORE UPDATE OR DELETE ON profiled_v2_proposals FOR EACH ROW EXECUTE FUNCTION refuse_profiled_v2_history_change();
CREATE TRIGGER profiled_v2_accept_immutable BEFORE UPDATE OR DELETE ON profiled_v2_accepts FOR EACH ROW EXECUTE FUNCTION refuse_profiled_v2_history_change();

UPDATE meta SET value='30' WHERE key='state_format_version';
