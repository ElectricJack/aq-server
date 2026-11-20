-- D10 (PB-04): opt-in record representation for bounded transitions.
-- Writer format 3 stores the State header (artifacts stripped) and its exact
-- accounting on the namespace row and every retained-history record in
-- state_records. Format 2 namespaces are unchanged; there is no in-place
-- migration of an existing namespace and no format downgrade. The 'null'
-- snapshot of a format 3 row is refused by every earlier writer's decoder.
ALTER TABLE aq_playbooks.namespaces DROP CONSTRAINT namespaces_writer_format_check;
ALTER TABLE aq_playbooks.namespaces
    ADD CONSTRAINT namespaces_writer_format_check CHECK(writer_format IN (2,3)),
    ADD COLUMN state_header bytea,
    ADD COLUMN state_accounting jsonb,
    ADD CONSTRAINT state_records_header_complete CHECK (
        (writer_format=3) = (state_header IS NOT NULL AND state_accounting IS NOT NULL)
        AND (writer_format<>3 OR (snapshot='null'::jsonb AND protection_key_id IS NULL))
    );

CREATE OR REPLACE FUNCTION aq_playbooks.check_namespace_writer() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'catalog namespace retirement is not implemented'; END IF;
    IF TG_OP='UPDATE' THEN
        IF NEW.writer_format<OLD.writer_format THEN
            RAISE EXCEPTION 'namespace writer format downgrade refused';
        END IF;
        IF (NEW.catalog_actual_definition_count,NEW.catalog_actual_activation_count)
            IS DISTINCT FROM (OLD.catalog_actual_definition_count,OLD.catalog_actual_activation_count)
            AND pg_trigger_depth()<>2 THEN
            RAISE EXCEPTION 'current catalog actual counts require an owned row transition';
        END IF;
        IF (NEW.catalog_definition_count,NEW.catalog_activation_count)
            IS DISTINCT FROM (OLD.catalog_definition_count,OLD.catalog_activation_count)
            AND NEW.revision<>OLD.revision+1 THEN
            RAISE EXCEPTION 'current catalog expected counts require a State boundary';
        END IF;
    END IF;
    IF NEW.writer_format NOT IN (2,3)
       OR current_setting('aq_playbooks.writer_namespace',true) IS DISTINCT FROM NEW.namespace
       OR current_setting('aq_playbooks.writer_format',true) IS DISTINCT FROM NEW.writer_format::text THEN
        RAISE EXCEPTION 'qualified current catalog namespace writer required';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION aq_playbooks.check_catalog_writer() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n text;
BEGIN
    IF TG_OP='UPDATE' AND (NEW.namespace,NEW.kind,NEW.item_key) IS DISTINCT FROM (OLD.namespace,OLD.kind,OLD.item_key) THEN
        RAISE EXCEPTION 'current catalog row identity is immutable';
    END IF;
    IF TG_OP='DELETE' THEN n:=OLD.namespace; ELSE n:=NEW.namespace; END IF;
    IF current_setting('aq_playbooks.writer_namespace',true) IS DISTINCT FROM n
       OR NOT EXISTS(SELECT 1 FROM aq_playbooks.namespaces WHERE namespace=n
            AND writer_format::text=current_setting('aq_playbooks.writer_format',true)
            AND owner_backend_pid=pg_backend_pid() AND owner IS NOT NULL) THEN
        RAISE EXCEPTION 'qualified current catalog owner writer required';
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;

CREATE TABLE aq_playbooks.state_records (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    field text COLLATE "C" NOT NULL,
    sort_key bytea NOT NULL,
    value bytea NOT NULL,
    PRIMARY KEY(namespace,field,sort_key)
);
CREATE FUNCTION aq_playbooks.check_record_writer() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n text;
BEGIN
    IF TG_OP='DELETE' THEN n:=OLD.namespace; ELSE n:=NEW.namespace; END IF;
    IF current_setting('aq_playbooks.writer_namespace',true) IS DISTINCT FROM n
       OR current_setting('aq_playbooks.writer_format',true) IS DISTINCT FROM '3' THEN
        RAISE EXCEPTION 'qualified record namespace writer required';
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER state_records_writer BEFORE INSERT OR UPDATE OR DELETE ON aq_playbooks.state_records
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.check_record_writer();
CREATE TRIGGER state_records_no_truncate BEFORE TRUNCATE ON aq_playbooks.state_records
    FOR EACH STATEMENT EXECUTE FUNCTION aq_playbooks.reject_artifact_mutation();
