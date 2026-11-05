"""Focused SBX24 Rust checks under aq test's foreground resource controls."""
import os
import json
from pathlib import Path
import re
import subprocess


def test_source_retention_gc(tmp_path):
    root = Path(__file__).resolve().parents[1]
    env = dict(os.environ, CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0",
               RUST_TEST_THREADS="1")
    assert env.get("DATABASE_URL"), "set DATABASE_URL via scripts/test-db"
    commands = [
        ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "source_retention::gc::tests"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--test", "profiled_results",
         "settlement_owner39::source_retention_gc::source_retention_gc_acceptance_and_max_age"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--test", "source_retention"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--test", "store", "migrations_are_idempotent"],
    ]
    evidence = Path(env.get("AQ_SOURCE_RETENTION_EVIDENCE", str(tmp_path)))
    evidence.mkdir(parents=True, exist_ok=True)
    results = []
    for index, command in enumerate(commands):
        log = evidence / f"{index:02d}.log"
        with log.open("w") as output:
            result = subprocess.run(command, cwd=root, env=env, stdout=output,
                                    stderr=subprocess.STDOUT)
        body = log.read_text()
        assert result.returncode == 0, body
        passed = re.search(r"test result: ok\. (\d+) passed", body)
        assert passed and int(passed[1]) > 0, body
        results.append({"command": command, "passed": int(passed[1]), "log": str(log)})
    (evidence / "results.json").write_text(json.dumps(results, indent=2) + "\n")
