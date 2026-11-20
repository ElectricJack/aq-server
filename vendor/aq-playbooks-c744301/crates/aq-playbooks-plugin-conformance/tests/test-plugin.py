#!/usr/bin/env python3
"""Fixture process for the conformance driver, with no external service."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

assert sys.argv[1:] == ["invoke"]
request = json.load(sys.stdin)
env = os.environ.copy()
# CPython can synthesize its own locale variable after exec.
env.pop("LC_CTYPE", None)
assert env == {
    "PATH": "/usr/local/bin:/usr/bin:/bin",
    "HOME": request["state_dir"],
    "TMPDIR": request["state_dir"] + "/tmp",
    "AQ_PLUGIN_PROTOCOL": "aq.plugin/1",
}
assert request["protocol"] == env["AQ_PLUGIN_PROTOCOL"]
assert request["project_id"] == "10000000-0000-4000-8000-000000000004"
assert Path.cwd() == Path(env["HOME"])
assert Path(env["TMPDIR"]).is_dir()
assert request["binding"] == {"message": "test fixture"}
assert Path(request["secret_paths"]["plugins/test/token"]).read_text() == "fake\n"
child_budget = 2_000 if request["command"] == "ext.test.ping_retry" else 4_000
assert 0 < request["deadline_ms"] <= child_budget

key = request["idempotency_key"].removeprefix("sha256:")
assert len(key) == 64 and all(char in "0123456789abcdef" for char in key)
action = request["input"]["action"]
cache = Path(key + ".json")
if action == "timeout_retry":
    with Path("retry-budgets").open("a") as record:
        record.write(str(request["deadline_ms"]) + "\n")
    marker = Path(key + ".first")
    if not marker.exists():
        marker.write_text("first")
        subprocess.Popen([sys.executable, "-c",
                            "import pathlib,time;time.sleep(2.2);pathlib.Path('orphan').touch()"])
        time.sleep(10)
if action == "exit":
    sys.exit(2)
if action == "malformed":
    print("{")
    sys.exit(0)

response = {
    "outcome": "ok",
    "output": {"text": "ok"},
    "events": [{"name": "ext.test.pinged", "payload": {
        "subject": {"id": "topic", "revision": 2}, "operation": "first"
    }}],
}
if action == "outcome":
    response["outcome"] = "unknown"
if action == "bytes":
    response["output"] = {"text": "ééé"}
if action == "forbidden_event":
    response["events"].append({"name": "aq.work.changed", "payload": {}})
if action == "bad_event":
    response["events"].append({"name": "ext.test.pinged", "payload": {
        "subject": {"id": "topic", "revision": -1}, "operation": "first"
    }})
if action == "unstable":
    repeats = cache.exists()
    cache.write_text("changed")
    print(json.dumps(response, indent=1 if repeats else None))
    sys.exit(0)
if not cache.exists():
    with Path("effects").open("a") as ledger:
        ledger.write(key + "\n")
    cache.write_text(json.dumps(response, separators=(",", ":")) + "\n")
sys.stdout.write(cache.read_text())
