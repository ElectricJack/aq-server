-- Editor metadata and inert Markdown source; runtime catalog and admission remain unchanged.
CREATE TABLE intelligence_structure (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton),
    revision BIGINT NOT NULL DEFAULT 0 CHECK(revision >= 0),
    document JSONB NOT NULL CHECK(jsonb_typeof(document) = 'object'),
    CHECK(octet_length(document->>'routing_markdown') <= 131072)
);
INSERT INTO intelligence_structure(singleton, document) VALUES(TRUE,
    '{"routing_markdown":"","parents":[{"id":"simple","description":""},{"id":"standard","description":""},{"id":"complex","description":""},{"id":"deep","description":""}],"assignments":[]}');
CREATE TABLE intelligence_structure_operations (
    actor_id UUID NOT NULL REFERENCES users(id),
    operation_id UUID NOT NULL,
    payload_digest TEXT NOT NULL,
    response JSONB NOT NULL,
    PRIMARY KEY(actor_id,operation_id)
);
UPDATE meta SET value='117' WHERE key='state_format_version';
