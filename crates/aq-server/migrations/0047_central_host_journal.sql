-- Private supervisor-host facts, never worker assignments or credential material.
CREATE TABLE central_host_launches (
 actor_id UUID NOT NULL,
 generation BIGINT NOT NULL,
 launch_id UUID NOT NULL UNIQUE,
 root TEXT NOT NULL CHECK(octet_length(root) BETWEEN 1 AND 4096),
 host_plan TEXT NOT NULL CHECK(octet_length(host_plan) BETWEEN 1 AND 65536),
 reservation TEXT NOT NULL CHECK(octet_length(reservation) BETWEEN 1 AND 65536),
 PRIMARY KEY(actor_id,generation),
 FOREIGN KEY(actor_id,generation) REFERENCES central_session_generations(actor_id,generation)
);
CREATE TABLE central_host_facts (
 launch_id UUID NOT NULL REFERENCES central_host_launches(launch_id),
 kind TEXT NOT NULL CHECK(kind IN ('intent','guardian','start','running','stopped','lost')),
 digest TEXT NOT NULL,
 body TEXT NOT NULL CHECK(octet_length(body) BETWEEN 1 AND 65536),
 PRIMARY KEY(launch_id,kind,digest),
 CHECK(digest=encode(sha256(convert_to(body,'UTF8')),'hex'))
);
CREATE UNIQUE INDEX central_host_single_intent ON central_host_facts(launch_id,kind)
 WHERE kind IN ('intent','guardian','start');
CREATE FUNCTION retain_central_host_fact() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'central host history is immutable';
END $$;
CREATE TRIGGER central_host_launch_immutable BEFORE UPDATE OR DELETE ON central_host_launches
 FOR EACH ROW EXECUTE FUNCTION retain_central_host_fact();
CREATE TRIGGER central_host_launch_no_truncate BEFORE TRUNCATE ON central_host_launches
 FOR EACH STATEMENT EXECUTE FUNCTION retain_central_host_fact();
CREATE TRIGGER central_host_fact_immutable BEFORE UPDATE OR DELETE ON central_host_facts
 FOR EACH ROW EXECUTE FUNCTION retain_central_host_fact();
CREATE TRIGGER central_host_fact_no_truncate BEFORE TRUNCATE ON central_host_facts
 FOR EACH STATEMENT EXECUTE FUNCTION retain_central_host_fact();
INSERT INTO meta(key,value) VALUES('state_format_version','47') ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value;
