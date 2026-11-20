-- Spec 0003, owner direction "rolling bounds": history is three rolling sets
-- of `policy.history_keep` each per namespace: finished runs (each with its
-- effects and receipts), the receipts of events (`event:<key>`), and admin
-- receipts. Each commit that writes history deletes the oldest of each set
-- beyond that number (a run's items before its run row), a bounded number
-- per commit. Oldest is by the namespace's sequence (a run's terminal
-- receipt, a receipt's own), never by host time.
--
-- Finding the oldest of each set reads one of these partial indexes in
-- order (no sort, no scan of the table).
CREATE INDEX run_history_runs_by_sequence ON aq_playbooks.run_history(namespace,sequence,item_key) WHERE kind='runs';
CREATE INDEX run_history_event_receipts_by_sequence ON aq_playbooks.run_history(namespace,sequence,item_key) WHERE kind='receipts' AND run_id LIKE 'event:%';
CREATE INDEX run_history_admin_receipts_by_sequence ON aq_playbooks.run_history(namespace,sequence,item_key) WHERE kind='receipts' AND run_id='admin';
-- How many each namespace's history holds of each set, maintained by the
-- same transactions that insert, trim and delete history rows, so the trim
-- never counts rows.
CREATE TABLE aq_playbooks.run_history_counts (
    namespace text PRIMARY KEY REFERENCES aq_playbooks.namespaces(namespace),
    runs bigint NOT NULL DEFAULT 0 CHECK(runs>=0),
    event_receipts bigint NOT NULL DEFAULT 0 CHECK(event_receipts>=0),
    admin_receipts bigint NOT NULL DEFAULT 0 CHECK(admin_receipts>=0)
);
-- Namespaces with history written before this migration (one count, once).
INSERT INTO aq_playbooks.run_history_counts(namespace,runs,event_receipts,admin_receipts)
    SELECT namespace,
           count(*) FILTER (WHERE kind='runs'),
           count(*) FILTER (WHERE kind='receipts' AND run_id LIKE 'event:%'),
           count(*) FILTER (WHERE kind='receipts' AND run_id='admin')
    FROM aq_playbooks.run_history GROUP BY namespace;
-- Written only by the namespace's leased writer, as run_history itself.
CREATE TRIGGER run_history_counts_writer BEFORE INSERT OR UPDATE OR DELETE ON aq_playbooks.run_history_counts
    FOR EACH ROW EXECUTE FUNCTION aq_playbooks.check_run_history_writer();
CREATE TRIGGER run_history_counts_no_truncate BEFORE TRUNCATE ON aq_playbooks.run_history_counts
    FOR EACH STATEMENT EXECUTE FUNCTION aq_playbooks.reject_run_history_truncate();
