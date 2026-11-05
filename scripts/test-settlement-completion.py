"""Focused completion snapshot/outbox regressions under aq test resource controls."""
import os
from pathlib import Path
import re
import subprocess


def test_settlement_completion():
    root = Path(__file__).resolve().parents[1]
    env = dict(os.environ, CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
               RUST_TEST_THREADS="1")
    assert env.get("DATABASE_URL"), "use scripts/test-db"
    evidence = root / ".aq/tmp/settlement-completion-tests"
    evidence.mkdir(parents=True, exist_ok=True)
    checks = [
        ("profiled_results", "settlement39_actual_owners_accept_one_combined_decision_hold_until_exact_graph_receipt"),
        ("profiled_results", "settlement_completion_outbox_replays_engine_acceptance_before_ack"),
        ("profiled_results", "settlement_http"),
        ("playbook_bridge", "outbox_replay_delivers_each_event_once"),
        ("store", "migrations_are_idempotent"),
    ]
    for index, (target, name) in enumerate(checks):
        command = ["cargo", "test", "--locked", "--offline", "-p", "aq-server",
                   "--test", target, name, "--", "--test-threads=1"]
        log = evidence / f"{index:02d}.log"
        with log.open("w") as output:
            result = subprocess.run(command, cwd=root, env=env, stdout=output,
                                    stderr=subprocess.STDOUT)
        body = log.read_text()
        assert result.returncode == 0, f"{' '.join(command)} failed; see {log}\n{body[-16000:]}"
        assert re.search(r"test result: ok\. [1-9]\d* passed", body), body
