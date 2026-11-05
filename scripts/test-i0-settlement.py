"""Focused I0 regressions under aq test's foreground resource semaphore.

Requires DATABASE_URL from scripts/test-db. Cargo logs are retained in
AQ_I0_EVIDENCE (default .aq/tmp/i0-settlement-tests).
"""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def test_i0_settlement_regressions():
    env = os.environ.copy()
    assert env.get("DATABASE_URL"), "use scripts/test-db"
    env.update(CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0", RUST_TEST_THREADS="1")
    evidence = Path(env.get("AQ_I0_EVIDENCE", ROOT / ".aq/tmp/i0-settlement-tests"))
    evidence.mkdir(parents=True, exist_ok=True)
    commands = [
        ["cargo", "test", "--locked", "--offline", "-p", "aq-server", "--lib", "definition_identity_conflict_has_named_native_diagnostic"],
        ["cargo", "test", "--locked", "--offline", "-p", "aq-server", "--lib", "terminal::runtime::"],
        ["cargo", "test", "--locked", "--offline", "-p", "aq-server", "--test", "config", "--test", "profiled_artifacts", "--test", "terminal_output_poll", "--test", "terminal_store"],
        ["cargo", "test", "--locked", "--offline", "-p", "aq-server", "--test", "profiled_results", "settlement_http"],
    ]
    for index, command in enumerate(commands):
        log = evidence / f"{index:02d}.log"
        with log.open("w") as output:
            result = subprocess.run(command, cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT)
        assert result.returncode == 0, f"{' '.join(command)} failed; see {log}\n{log.read_text()[-12000:]}"
