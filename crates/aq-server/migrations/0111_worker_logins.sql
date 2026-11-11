-- Delivery requests and receipts contain provider names only, never login material.
CREATE TABLE worker_login_deliveries (
    node_id uuid PRIMARY KEY REFERENCES nodes(id),
    generation uuid,
    providers text[] NOT NULL DEFAULT '{}',
    status text NOT NULL DEFAULT 'waiting' CHECK (status IN ('waiting','pending','applied','failed')),
    requested_at_ms bigint,
    applied_at_ms bigint,
    receiver_seen_at_ms bigint,
    actor_id uuid,
    CHECK ((generation IS NULL) = (requested_at_ms IS NULL))
);
UPDATE meta SET value='111' WHERE key='state_format_version';

CREATE TABLE worker_login_operations (
    operation_id uuid PRIMARY KEY,
    node_id uuid NOT NULL REFERENCES nodes(id),
    actor_id uuid NOT NULL REFERENCES users(id),
    providers text[] NOT NULL
);
