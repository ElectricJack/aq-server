-- New snapshots retain immutable keyed admission receipts. A 0.1.0 writer
-- ignores that field, so fence both a fresh old constructor (migration version)
-- and an already-open old connection before it can overwrite protected bytes.
ALTER TABLE aq_playbooks.namespaces
    ADD COLUMN writer_format integer NOT NULL DEFAULT 0 CHECK (writer_format IN (0, 1));

CREATE FUNCTION aq_playbooks.check_namespace_writer() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF OLD.writer_format >= 1 THEN
            RAISE EXCEPTION 'keyed admission namespace retirement is not implemented';
        END IF;
        RETURN OLD;
    END IF;
    IF TG_OP = 'UPDATE' AND NEW.writer_format < OLD.writer_format THEN
        RAISE EXCEPTION 'namespace writer format downgrade refused';
    END IF;
    IF NEW.writer_format >= 1 OR (TG_OP = 'UPDATE' AND OLD.writer_format >= 1) THEN
        IF current_setting('aq_playbooks.writer_namespace', true) IS DISTINCT FROM NEW.namespace
           OR current_setting('aq_playbooks.writer_format', true) IS DISTINCT FROM '1' THEN
            RAISE EXCEPTION 'qualified keyed admission namespace writer required';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER namespace_writer_format
    BEFORE INSERT OR UPDATE OR DELETE ON aq_playbooks.namespaces
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.check_namespace_writer();
