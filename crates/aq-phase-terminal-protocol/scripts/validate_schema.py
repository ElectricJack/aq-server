"""Validate finite published shape vectors; relational checks remain Rust-only."""
import json
from pathlib import Path
import jsonschema

root = Path(__file__).resolve().parent.parent / "artifacts"
schema = json.loads((root / "schema.json").read_text())
fixtures = json.loads((root / "fixtures.json").read_text())
validators = {}
for family, name in {
    "started_request": "StartedRequest", "stopped_request": "StoppedRequest",
    "closure_request": "ClosureRequest", "started_receipt": "StartedReceipt",
    "stopped_receipt": "StoppedReceipt", "closure_receipt": "ClosureReceipt",
    "history_request": "HistoryRequest", "history_receipt": "HistoryReceipt",
    "historical_readback": "HistoricalReadback",
}.items():
    document = dict(schema)
    document["$ref"] = f"#/$defs/{name}"
    document.pop("required", None)
    document.pop("properties", None)
    document.pop("additionalProperties", None)
    validators[family] = jsonschema.Draft202012Validator(document)
count = 0
for key in ["started_request", "stopped_request", "started_receipt", "stopped_receipt",
            "history_request", "historical_readback"]:
    validators[key].validate(fixtures[key])
    count += 1
for key in ["history_known", "history_absent"]:
    validators["history_receipt"].validate(fixtures[key])
    count += 1
for key, family in [("closure_requests", "closure_request"), ("closure_receipts", "closure_receipt")]:
    for value in fixtures[key]:
        validators[family].validate(value)
        count += 1
invalid = json.loads((root / "invalid.json").read_text())["invalid"]
shapes = 0
for vector in invalid:
    if not vector["requires_relational_validation"]:
        assert list(validators[vector["family"]].iter_errors(vector["document"])), vector["case"]
        shapes += 1
print(f"PASS {count} valid documents; {shapes} invalid shapes; {len(invalid)-shapes} relational negatives delegated to Rust")
