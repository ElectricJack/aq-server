#!/usr/bin/env python3
"""Validate retained synthetic actual-route envelopes; no service connection."""
import json
from pathlib import Path
import jsonschema

root = Path(__file__).resolve().parent
schema = json.loads((root / "schema.json").read_text())
jsonschema.Draft202012Validator.check_schema(schema)
for name in ("bootstrap", "overview", "sessions", "session", "projects"):
    body = json.loads((root / "fixtures" / f"{name}.json").read_text())
    jsonschema.validate(body, schema)
    assert body["api_version"] == "dashboard-read-1"
    if name == "bootstrap":
        assert all(c["version"] == "1" for c in body["data"]["capabilities"])
        assert body["data"]["product"]["dashboard_digest"] is None
print("5 actual-route synthetic fixtures match dashboard-read-1 schema")
