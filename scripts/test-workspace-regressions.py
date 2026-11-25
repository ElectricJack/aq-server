"""Run the recorded Rust regressions inside aq test's foreground resource slot.

AQ_WORKSPACE_REGRESSION_SCOPE selects exact (default), area, workspace or checks.
AQ_WORKSPACE_REGRESSION_EVIDENCE names a new evidence directory for each run.
Requires the disposable DATABASE_URL and the pinned actual Graph fixture.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
BASELINE = ROOT / "docs/implementation/A3b-checks/persistent-failure-comparison.json"
# The recorded `<target>::<test>` names are module paths inside these grouped
# test binaries (crates/aq-server/tests/<binary>/main.rs).
BINARY = {
    "dashboard_aggregation": "it_dashboard",
    "native_accepted_preparation": "it_native",
    "native_cancellation": "it_native",
    "native_expiry": "it_native",
    "phase_execution_transport": "it_phase",
    "phase_never_granted": "it_phase",
    "phase_terminal_closure": "it_phase",
    "profiled_hot": "it_profiled",
    "schema30_preparation": "it_schema",
    "schema32_phase_execution": "it_schema",
    "schema33_phase_terminal": "it_schema",
    "schema43_cancellation": "it_schema",
}


RESOURCE_CONTROLS = {
    "CARGO_BUILD_JOBS": "2",
    "RUST_TEST_THREADS": "1",
    "CARGO_PROFILE_DEV_DEBUG": "0",
    "CARGO_PROFILE_TEST_DEBUG": "0",
    "CARGO_INCREMENTAL": "0",
}


def qualification_environment(scope):
    # Keep the default optimization/features while bounding build storage.
    environment = dict(os.environ, **RESOURCE_CONTROLS)
    if scope != "checks":
        assert environment.get("DATABASE_URL"), "use scripts/test-db"
        binary = Path(environment["AQ_TEST_GRAPH_BINARY"])
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == environment["AQ_TEST_GRAPH_SHA256"]
    return environment


def binary_flags(targets):
    return [arg for binary in sorted({BINARY[target] for target in targets}) for arg in ("--test", binary)]


def exact_commands(expected, targets):
    base = ["cargo", "test", "--locked", "-p", "aq-server"]
    yield "compile", base + ["--no-run"] + binary_flags(targets)
    for name in expected:
        target = name.split("::", 1)[0]
        yield name, base + [
            "--test", BINARY[target], name, "--", "--exact", "--test-threads=1", "--nocapture",
        ]


def area_command(targets):
    area = sorted(set(targets) | {"dashboard_aggregation", "native_cancellation", "phase_never_granted", "native_expiry"})
    command = ["cargo", "test", "--locked", "-p", "aq-server", "--no-fail-fast"]
    return command + binary_flags(area) + ["--", "--test-threads=1"] + [f"{target}::" for target in area]


def qualification_commands(scope, expected):
    targets = sorted({name.split("::", 1)[0] for name in expected})
    if scope == "exact":
        return list(exact_commands(expected, targets))
    if scope == "area":
        return [("area", area_command(targets))]
    if scope == "workspace":
        return [("workspace", [
            "cargo", "test", "--locked", "--workspace", "--no-fail-fast",
            "--", "--test-threads=2",
        ])]
    return [
        ("format", ["cargo", "fmt", "--check"]),
        ("clippy", ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"]),
    ]


def evidence_header(scope, environment, expected):
    results = {
        "scope": scope,
        "source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD"], cwd=ROOT)).hexdigest(),
        "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "resource_controls": {key: environment[key] for key in RESOURCE_CONTROLS},
        "graph_sha256": environment.get("AQ_TEST_GRAPH_SHA256"),
        "expected_regressions": expected,
        "runs": [],
    }
    if scope != "checks":
        database = urlsplit(environment["DATABASE_URL"])
        results["database_endpoint"] = {"host": database.hostname, "port": database.port}
    return results


def regression_outcomes(name, lines, expected):
    outcomes = {}
    grouped = "::" in name
    for line in lines:
        heading = re.search(r"Running tests/(\w+)/main\.rs ", line)
        if heading:
            grouped = heading[1] in BINARY.values()
        if not grouped:
            continue
        result = re.fullmatch(r"test (\S+) \.\.\. (ok|FAILED|ignored)", line)
        if result and result[1] in expected:
            outcomes[result[1]] = result[2]
    return outcomes


def required_cases(name, scope, expected):
    if name in expected:
        return [name]
    if scope in {"area", "workspace"}:
        return expected
    return []


def run_qualification(log, command, environment):
    started = time.monotonic()
    with log.open("w") as output:
        completed = subprocess.run(command, cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT)
    lines = log.read_text().splitlines()
    return completed.returncode, started, lines


def test_workspace_regressions():
    scope = os.environ.get("AQ_WORKSPACE_REGRESSION_SCOPE", "exact")
    assert scope in {"exact", "area", "workspace", "checks"}
    evidence = Path(os.environ["AQ_WORKSPACE_REGRESSION_EVIDENCE"])
    evidence.mkdir(parents=True, exist_ok=False)
    environment = qualification_environment(scope)
    baseline = json.loads(BASELINE.read_text())
    expected = [case["test"] for case in baseline]
    assert len(expected) == len(set(expected)) == 19
    commands = qualification_commands(scope, expected)
    results = evidence_header(scope, environment, expected)
    for index, (name, command) in enumerate(commands):
        log = evidence / f"{index:02d}-{name.split('::')[0]}.txt"
        exit_code, started, lines = run_qualification(log, command, environment)
        outcomes = regression_outcomes(name, lines, expected)
        required = required_cases(name, scope, expected)
        results["runs"].append({
            "case": name, "command": command, "exit_code": exit_code,
            "seconds": round(time.monotonic() - started, 3), "log": log.name,
            "test_results": [line for line in lines if line.startswith("test result:")],
            "regression_outcomes": outcomes,
            "missing_or_failed_regressions": [case for case in required if outcomes.get(case) != "ok"],
        })
        (evidence / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(f"{name}: exit {exit_code}; {log}", flush=True)
        if name == "compile" and exit_code:
            break
    failures = [run for run in results["runs"] if run["exit_code"] or run["missing_or_failed_regressions"]]
    assert not failures, json.dumps(failures, indent=2)
