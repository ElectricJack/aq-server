-- Snapshot only server-owned creation intents. Until the authenticated journal
-- confirms the corresponding task, a snapshot is never exposed to readers.
CREATE TABLE task_git_authors (
    source TEXT NOT NULL CHECK (source IN ('human_graph', 'dashboard_graph')),
    operation_id UUID NOT NULL,
    project_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    task_id UUID NOT NULL,
    requester_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (name <> ''),
    email TEXT NOT NULL CHECK (email <> ''),
    inherited_from UUID,
    confirmed BOOLEAN NOT NULL DEFAULT TRUE,
    PRIMARY KEY (source, operation_id, task_id)
);
CREATE UNIQUE INDEX task_git_authors_confirmed
    ON task_git_authors(project_id, workspace_id, task_id) WHERE confirmed;

-- The canonical email is retained verbatim. AQ does not store display names.
CREATE FUNCTION task_git_account_name(email TEXT) RETURNS TEXT
    LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT COALESCE(NULLIF(left(split_part(email, '@', 1), 120), ''), 'AQ task requester');
$$;

CREATE FUNCTION capture_human_task_git_author(intent human_graph_operations) RETURNS VOID
    LANGUAGE sql AS $$
    INSERT INTO task_git_authors (source, operation_id, project_id, workspace_id,
        task_id, requester_id, name, email, confirmed)
    SELECT 'human_graph', intent.operation_id, intent.project_id, intent.workspace_id,
        intent.task_id, u.id, task_git_account_name(u.email), u.email, FALSE
    FROM users u WHERE u.id = intent.actor_id AND u.email <> ''
    ON CONFLICT DO NOTHING;
$$;

CREATE FUNCTION sync_human_task_git_author() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        PERFORM capture_human_task_git_author(NEW);
    END IF;
    IF NEW.status->>'state' = 'confirmed'
        AND NEW.raw_receipt->>'change_set_id' IS NOT NULL
        AND NEW.raw_receipt->'results'->0->>'id' = NEW.task_id::text
        AND NEW.raw_receipt->'results'->0->>'kind' = 'task'
        AND NEW.raw_receipt->'results'->0->>'workspace_id' = NEW.workspace_id::text THEN
        -- Also covers an operation admitted before the migration. An exact
        -- retained confirmation is required for this legacy path.
        PERFORM capture_human_task_git_author(NEW);
        UPDATE task_git_authors SET confirmed = TRUE
        WHERE source = 'human_graph' AND operation_id = NEW.operation_id AND NOT confirmed;
    END IF;
    IF NEW.status->>'state' IN ('confirmed','conflict','rejected') THEN
        DELETE FROM task_git_authors
        WHERE source = 'human_graph' AND operation_id = NEW.operation_id AND NOT confirmed;
    END IF;
    RETURN NULL;
END $$;

CREATE TRIGGER human_task_git_author
AFTER INSERT OR UPDATE OF status ON human_graph_operations
FOR EACH ROW EXECUTE FUNCTION sync_human_task_git_author();

CREATE FUNCTION capture_dashboard_task_git_authors(target_operation UUID, inherit_children BOOLEAN) RETURNS VOID
    LANGUAGE sql AS $$
    WITH RECURSIVE intent AS MATERIALIZED (
        SELECT i.project_id, i.workspace_id, d.owner_id, convert_from(i.body, 'UTF8')::jsonb AS body
        FROM dashboard_graph_intents i JOIN dashboard_operations d USING (operation_id)
        WHERE i.operation_id = target_operation
          AND d.kind IN ('task.create','task.graph.create','epic.retirement.register')
          AND (d.owner_id IS NOT NULL OR (inherit_children AND d.kind IN ('task.create','task.graph.create')))
          AND d.scope = jsonb_build_object('kind','project','project_id',i.project_id)
    ), operations AS MATERIALIZED (
        SELECT op FROM intent CROSS JOIN LATERAL jsonb_array_elements(body->'operations') op
    ), tasks AS (
        SELECT (op->>'id')::uuid AS task FROM operations
        WHERE op->>'op' = 'create_node' AND op->>'kind' = 'task'
    ), parents AS (
        SELECT (op->>'target')::uuid AS child, (min(op->>'source'))::uuid AS parent
        FROM operations
        WHERE op->>'op' = 'add_edge' AND op->>'edge_type' = 'parent-child'
        GROUP BY op->>'target' HAVING count(*) = 1
    ), ancestors(task, ancestor, immediate_parent, visited) AS (
        SELECT t.task, p.parent, p.parent, ARRAY[t.task,p.parent]
        FROM tasks t JOIN parents p ON p.child = t.task CROSS JOIN intent i
        WHERE i.owner_id IS NULL
        UNION ALL
        SELECT a.task, p.parent, a.immediate_parent, a.visited || p.parent
        FROM ancestors a JOIN tasks t ON t.task = a.ancestor JOIN parents p ON p.child = t.task
        WHERE NOT p.parent = ANY(a.visited)
    ), authors AS (
        SELECT t.task, u.id AS requester_id, task_git_account_name(u.email) AS name,
            u.email, NULL::uuid AS inherited_from
        FROM tasks t CROSS JOIN intent i JOIN users u ON u.id = i.owner_id AND u.email <> ''
        UNION ALL
        SELECT a.task, saved.requester_id, saved.name, saved.email, a.immediate_parent
        FROM ancestors a CROSS JOIN intent i
        JOIN task_git_authors saved ON saved.project_id = i.project_id AND saved.workspace_id = i.workspace_id
        WHERE saved.task_id = a.ancestor AND saved.confirmed
          AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.task = a.ancestor)
    )
    INSERT INTO task_git_authors (source,operation_id,project_id,workspace_id,
        task_id,requester_id,name,email,inherited_from,confirmed)
    SELECT 'dashboard_graph',target_operation,i.project_id,i.workspace_id,a.task,a.requester_id,a.name,
        a.email,a.inherited_from,FALSE
    FROM authors a CROSS JOIN intent i
    ON CONFLICT DO NOTHING;
$$;

CREATE FUNCTION snapshot_dashboard_task_git_authors() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    PERFORM capture_dashboard_task_git_authors(NEW.operation_id, TRUE);
    RETURN NULL;
END $$;

CREATE TRIGGER dashboard_task_git_author_snapshot AFTER INSERT ON dashboard_graph_intents
FOR EACH ROW EXECUTE FUNCTION snapshot_dashboard_task_git_authors();

CREATE FUNCTION confirm_dashboard_task_git_authors(target_operation UUID, receipt JSONB) RETURNS VOID
    LANGUAGE sql AS $$
    UPDATE task_git_authors a SET confirmed = TRUE
    WHERE a.source = 'dashboard_graph' AND a.operation_id = target_operation AND NOT a.confirmed
      AND (receipt->'receipt_refs') @> '[{"type":"graph_change_set"}]'
      AND (receipt->'affected_entities') @> jsonb_build_array(jsonb_build_object('type','task','id',a.task_id));
$$;

CREATE FUNCTION sync_dashboard_task_git_authors() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.disposition = 'confirmed' THEN
        IF NEW.owner_id IS NOT NULL AND NOT EXISTS (
            SELECT 1 FROM task_git_authors WHERE source = 'dashboard_graph' AND operation_id = NEW.operation_id
        ) THEN
            PERFORM capture_dashboard_task_git_authors(NEW.operation_id, FALSE);
        END IF;
        PERFORM confirm_dashboard_task_git_authors(NEW.operation_id, NEW.response);
    END IF;
    DELETE FROM task_git_authors
    WHERE source = 'dashboard_graph' AND operation_id = NEW.operation_id AND NOT confirmed;
    RETURN NULL;
END $$;

CREATE TRIGGER dashboard_task_git_author_confirmation AFTER UPDATE OF disposition ON dashboard_operations
FOR EACH ROW
WHEN (OLD.disposition IS DISTINCT FROM NEW.disposition
    AND NEW.kind IN ('task.create','task.graph.create','epic.retirement.register')
    AND NEW.disposition IN ('confirmed','conflict','rejected'))
EXECUTE FUNCTION sync_dashboard_task_git_authors();

-- Materialize legacy attribution only from the authenticated, retained
-- confirmation and matching dispatch intent. The offers lookup stays indexed.
-- Legacy service actions have no admission-time parent snapshot.
INSERT INTO task_git_authors (source,operation_id,project_id,workspace_id,task_id,requester_id,name,email)
SELECT 'human_graph',h.operation_id,h.project_id,h.workspace_id,h.task_id,u.id,task_git_account_name(u.email),u.email
FROM human_graph_operations h JOIN users u ON u.id = h.actor_id AND u.email <> ''
WHERE h.status->>'state' = 'confirmed' AND h.raw_receipt->>'change_set_id' IS NOT NULL
  AND h.raw_receipt->'results'->0->>'id' = h.task_id::text
  AND h.raw_receipt->'results'->0->>'kind' = 'task'
  AND h.raw_receipt->'results'->0->>'workspace_id' = h.workspace_id::text;

DO $$
DECLARE item RECORD;
BEGIN
    FOR item IN SELECT d.operation_id,d.response
        FROM dashboard_operations d JOIN dashboard_graph_intents i USING (operation_id)
        WHERE d.disposition = 'confirmed' AND d.owner_id IS NOT NULL
          AND d.kind IN ('task.create','task.graph.create','epic.retirement.register')
          AND d.response->'receipt_refs' @> '[{"type":"graph_change_set"}]'
        ORDER BY d.created_at,d.operation_id
    LOOP
        PERFORM capture_dashboard_task_git_authors(item.operation_id, FALSE);
        PERFORM confirm_dashboard_task_git_authors(item.operation_id, item.response);
        DELETE FROM task_git_authors
        WHERE source = 'dashboard_graph' AND operation_id = item.operation_id AND NOT confirmed;
    END LOOP;
END $$;

UPDATE meta SET value='162' WHERE key='state_format_version';
