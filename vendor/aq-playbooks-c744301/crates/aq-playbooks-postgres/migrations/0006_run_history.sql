-- Spec 0003 (live state only): a settled cohort leaves the State in the commit
-- that settles it, and that same commit inserts its public inspection items
-- here, one row per run, effect or receipt item. Rows hold public projections only
-- (never encrypted), are inserted once and never updated, and are deleted by
-- the namespace owner by finish time. A commit never leaves an item key both
-- here and live (checked by the writer), so inspection needs no de-duplication.
CREATE TABLE aq_playbooks.run_history (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    kind text NOT NULL CHECK(kind IN ('runs','effects','receipts')),
    item_key text COLLATE "C" NOT NULL,
    run_id text NOT NULL,
    root_id text NOT NULL,
    sequence bigint NOT NULL CHECK(sequence>=0),
    finished_at_ms bigint NOT NULL,
    public_value jsonb NOT NULL,
    PRIMARY KEY(namespace,kind,item_key)
);
-- Inspection filters are kind, optional run and an item_key cursor: the
-- primary key serves unfiltered pages, run_history_by_run pages of one run.
CREATE INDEX run_history_by_run ON aq_playbooks.run_history(namespace,kind,run_id,item_key);
CREATE INDEX run_history_by_finish ON aq_playbooks.run_history(namespace,finished_at_ms);

CREATE FUNCTION aq_playbooks.reject_run_history_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'run history rows are immutable';
END;
$$;
CREATE FUNCTION aq_playbooks.reject_run_history_truncate() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'run_history is insert-only';
END;
$$;
-- Deleting history is destructive, so it also needs the session that holds
-- the namespace lease (as for catalog_rows).
CREATE FUNCTION aq_playbooks.check_run_history_writer() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n text;
BEGIN
    IF TG_OP='DELETE' THEN n:=OLD.namespace; ELSE n:=NEW.namespace; END IF;
    IF current_setting('aq_playbooks.writer_namespace',true) IS DISTINCT FROM n
       OR (TG_OP='DELETE' AND NOT EXISTS(SELECT 1 FROM aq_playbooks.namespaces WHERE namespace=n
            AND owner_backend_pid=pg_backend_pid() AND owner IS NOT NULL)) THEN
        RAISE EXCEPTION 'qualified run history namespace writer required';
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER run_history_immutable BEFORE UPDATE ON aq_playbooks.run_history
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.reject_run_history_update();
CREATE TRIGGER run_history_writer BEFORE INSERT OR DELETE ON aq_playbooks.run_history
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.check_run_history_writer();
CREATE TRIGGER run_history_no_truncate BEFORE TRUNCATE ON aq_playbooks.run_history
    FOR EACH STATEMENT EXECUTE FUNCTION aq_playbooks.reject_run_history_truncate();
