#!/usr/bin/env python3
"""Check the worker-machine/v1 fixtures against schema.json."""
import copy
import json
from pathlib import Path

import jsonschema

HERE = Path(__file__).resolve().parent
schema = json.loads((HERE / "schema.json").read_text())
jsonschema.Draft202012Validator.check_schema(schema)
fixtures = sorted((HERE / "fixtures").glob("*.json"))
assert len(fixtures) == 2, fixtures
for path in fixtures:
    jsonschema.validate(json.loads(path.read_text()), schema)

base = json.loads(fixtures[0].read_text())


def invalid(edit):
    body = copy.deepcopy(base)
    edit(body)
    try:
        jsonschema.validate(body, schema)
    except jsonschema.ValidationError:
        return
    raise AssertionError("expected an invalid report")


invalid(lambda b: b.update(cpu_busy_bps=10001))
invalid(lambda b: b.update(source="windows"))
invalid(lambda b: b.update(extra=1))
print("worker-machine-1: 2 fixtures and 3 invalid cases checked")
