#!/usr/bin/env python3
"""Independent canonical private-record/protocol vector; no services or secrets."""
import hashlib
import json
from pathlib import Path
root = Path(__file__).resolve().parents[1]
artifacts = root / "artifacts"
record = json.loads((artifacts / "scoped-presence-private-record.fixture.json").read_text())
report = json.loads((artifacts / "scoped-presence-report.fixture.json").read_text())
canonical = json.dumps(record, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
digest = hashlib.sha256(canonical).hexdigest()
assert report["private_binding"]["digest"] == digest
qualification = json.loads((artifacts / "scoped-presence-qualification.fixture.json").read_text())
qualification_digest = hashlib.sha256(json.dumps(qualification, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()
assert report["negotiation"]["qualification"]["digest"] == qualification_digest
assert report["presence"]["installation"] == record
assert record["clock_source"] == "CLOCK_BOOTTIME"
assert record["retrieval_deadline_ns"] == record["sent_ns"] + record["delivery"]["clock"]["valid_for_ms"] * 1_000_000
assert report["presence"]["observed_material_digest"] == record["delivery"]["lease"]["material_digest"]
assert report["presence"]["observed_material_bytes"] == record["delivery"]["lease"]["material_bytes"]
assert record["material_synced_ns"] < record["retrieval_deadline_ns"] < report["observation"]["read_started_ns"]
print(json.dumps({"schema_version": 1, "private_record_sha256": digest, "private_record_bytes": len(canonical)}, sort_keys=True))
