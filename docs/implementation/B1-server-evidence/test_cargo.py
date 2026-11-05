"""Run Cargo checks through AQ's pytest-only resource gate.

Select B1_SERVER_CHECK=conformance (default), legacy, endpoints, registration, endpoint_retry, postmerge, or format.
"""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
COMMANDS = {
    "conformance": ["cargo", "test", "--locked", "--workspace", "permit_fixture_conformance"],
    "legacy": ["cargo", "test", "--locked", "-p", "aq-admission@0.1.11", "--test", "transport", "--test", "preparation_v2", "--test", "preparation_refresh", "--test", "granted_closure"],
    "endpoints": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "native_registration", "--test", "native_initial_grant"],
    "registration": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "native_registration"],
    "endpoint_retry": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "native_initial_grant", "published::exact_full_artifact_and_material_receipt_coverage_is_mandatory", "--", "--exact"],
    "postmerge": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "permit_fixture_conformance"],
    "format": ["cargo", "fmt", "-p", "aq-server", "--check"],
}


def test_cargo():
    check = os.environ.get("B1_SERVER_CHECK", "conformance")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment.setdefault("CARGO_INCREMENTAL", "0")
    environment.setdefault("RUST_TEST_THREADS", "2")
    log = Path(__file__).with_name(f"{check}.txt")
    with log.open("w") as output:
        result = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment,
                                stdout=output, stderr=subprocess.STDOUT)
    raw = log.read_bytes()
    if raw:
        log.write_bytes(raw.rstrip(b"\r\n") + b"\n")
    assert result.returncode == 0, f"{COMMANDS[check]} exited {result.returncode}; see {log}"
