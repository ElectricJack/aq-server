"""Focused reservation-consumption Rust checks through the AQ resource gate."""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
COMMANDS = {
    "turns": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "supervisor_turns", "--", "--test-threads=1"],
    "area": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--lib", "central37", "--", "--test-threads=1"],
    "reservations": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--lib", "agent_access::messaging::global::reservations::tests::", "--", "--test-threads=1"],
    "conversation": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "supervisor_conversation"],
    "related": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "central_sessions", "--test", "supervisor_conversation", "--", "--test-threads=1"],
    "sessions": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "central_sessions"],
    "version": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--lib", "state_format_is_the_latest_migration"],
}


def test_cargo():
    check = os.environ.get("F1B_CONSUMPTION_CHECK", "turns")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment["CARGO_INCREMENTAL"] = "0"
    environment.setdefault("RUST_TEST_THREADS", "2")
    environment["PATH"] = str(Path.home() / ".cargo/bin") + os.pathsep + environment.get("PATH", "")
    log = Path(__file__).with_name(f"{check}.txt")
    with log.open("w") as output:
        result = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT)
    log.write_text(log.read_text().rstrip() + "\n")
    assert result.returncode == 0, f"{COMMANDS[check]} exited {result.returncode}; see {log}"
