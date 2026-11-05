-- REVIEW PROPOSAL ONLY. Not a migration; no schema number/meta update yet.
-- Apply only after root allocates 36 following the conversation 35 freeze.
-- SQL owner functions/triggers and crash/race qualification are implementation work.

CREATE TABLE result_media_budgets (
 scope_key TEXT PRIMARY KEY CHECK(scope_key='installation' OR scope_key ~ '^project/[0-9a-f-]{36}$'),
 limit_bytes BIGINT NOT NULL CHECK(limit_bytes BETWEEN 1 AND 268435456),
 reserved_bytes BIGINT NOT NULL DEFAULT 0 CHECK(reserved_bytes >= 0),
 retained_bytes BIGINT NOT NULL DEFAULT 0 CHECK(retained_bytes >= 0),
 historical_intents BIGINT NOT NULL DEFAULT 0 CHECK(historical_intents >= 0),
 CHECK(reserved_bytes + retained_bytes <= limit_bytes)
);

CREATE TABLE result_media_intents (
 operation_id UUID PRIMARY KEY,
 actor_id UUID NOT NULL REFERENCES users(id),
 report_id UUID NOT NULL REFERENCES profiled_result_reports(report_id),
 report_revision TEXT NOT NULL CHECK(report_revision ~ '^[0-9a-f]{64}$'),
 project_id UUID NOT NULL,
 -- Immutable server-derived report/task/attempt/source projection; never task truth.
 report_binding JSONB NOT NULL,
 storage_id UUID NOT NULL,
 rendering_policy TEXT NOT NULL CHECK(rendering_policy IN ('aq.png.rgba8-srgb-v1','aq.mp4.passthrough-v1')),
 admission_limits JSONB NOT NULL,
 request_bytes BYTEA NOT NULL CHECK(octet_length(request_bytes) BETWEEN 1 AND 16384),
 request_digest TEXT NOT NULL CHECK(request_digest='sha256:' || encode(sha256(request_bytes),'hex')),
 input_digest TEXT NOT NULL CHECK(input_digest ~ '^sha256:[0-9a-f]{64}$'),
 input_bytes BIGINT NOT NULL CHECK(input_bytes BETWEEN 1 AND CASE WHEN rendering_policy='aq.mp4.passthrough-v1' THEN 4194304 ELSE 1048576 END),
 filename TEXT NOT NULL CHECK(octet_length(filename) BETWEEN 1 AND 256),
 capture JSONB,
 state TEXT NOT NULL CHECK(state IN ('reserved','received','finalizing','retained','rejected','expired')),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms > 0),
 upload_expires_at_ms BIGINT NOT NULL,
 retain_until_ms BIGINT NOT NULL,
 reserved_bytes BIGINT NOT NULL CHECK(reserved_bytes BETWEEN 0 AND 10485760),
 retained_bytes BIGINT NOT NULL DEFAULT 0 CHECK(retained_bytes BETWEEN 0 AND 10485760),
 lease_id UUID,
 lease_expires_at_ms BIGINT,
 processing_attempts INTEGER NOT NULL DEFAULT 0 CHECK(processing_attempts BETWEEN 0 AND 8),
 received_at_ms BIGINT,
 rejection TEXT CHECK(rejection IN ('invalid_png','unsupported_png','input_mismatch','pixel_limit','processing_limit','storage_failure','invalid_video','unsupported_video','poster_unavailable')),
 receipt_bytes BYTEA CHECK(octet_length(receipt_bytes) BETWEEN 1 AND 16384),
 receipt_digest TEXT,
 -- Private generated file generation, never a caller path or public field.
 file_generation UUID,
 withheld_at_ms BIGINT,
 cleanup_state TEXT NOT NULL DEFAULT 'none' CHECK(cleanup_state IN ('none','pending','running','blocked','done')),
 cleanup_generation UUID,
 cleanup_completed_at_ms BIGINT,
 CHECK(upload_expires_at_ms > created_at_ms AND upload_expires_at_ms <= created_at_ms + 1800000),
 CHECK(retain_until_ms > upload_expires_at_ms AND retain_until_ms <= created_at_ms + 604800000),
 CHECK((lease_id IS NULL) = (lease_expires_at_ms IS NULL)),
 CHECK((receipt_bytes IS NULL) = (receipt_digest IS NULL)),
 CHECK(receipt_digest IS NULL OR receipt_digest='sha256:' || encode(sha256(receipt_bytes),'hex')),
 CHECK((state='retained') = (receipt_bytes IS NOT NULL)),
 CHECK(state<>'retained' OR (file_generation IS NOT NULL AND reserved_bytes=0)),
 CHECK(cleanup_state<>'done' OR (reserved_bytes=0 AND retained_bytes=0 AND cleanup_completed_at_ms IS NOT NULL))
);

CREATE INDEX result_media_exact_report ON result_media_intents(report_id,report_revision,operation_id)
 WHERE state='retained';
CREATE INDEX result_media_actor_lookup ON result_media_intents(actor_id,operation_id);
CREATE INDEX result_media_pending_cleanup ON result_media_intents(upload_expires_at_ms,operation_id)
 WHERE state IN ('reserved','received','finalizing','rejected','expired');
CREATE INDEX result_media_retained_cleanup ON result_media_intents(retain_until_ms,operation_id)
 WHERE state='retained' AND cleanup_state<>'done';

-- Required owner guard functions in the allocated migration:
-- 1. Immutable intent identity/raw bytes/report binding/storage/policy/deadlines.
-- 2. Once present, exact canonical receipt bytes/digest are immutable forever.
-- 3. Monotonic fenced state/lease/cleanup transitions; no retained -> uploading.
-- 4. Install then project budget locks; per-report capacity under actual result
--    authority; reserve10MiB before files and adjust to verified physical bytes.
-- 5. Quota release once, only after physical absence and no active writer.
-- 6. Receipt/intent linkage including actor/report/revision/input/variant bounds.
-- No DELETE/UPDATE path may erase identity/receipt or synthesize a media grant.
