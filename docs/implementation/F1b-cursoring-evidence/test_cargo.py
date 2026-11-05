"""Focused F1b reply-cursoring checks (steady-harbor-87.2.4) through AQ's pytest resource gate."""
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
ACCEPT = "owner_message_reaches_supervisor_and_reply_is_recorded"
COMMANDS = {
    "check": ["cargo", "check", "--locked", "-p", "aq-server", "-p", "aq-supervisor-host", "--tests", "--features", "aq-server/test-support"],
    "acceptance": ["cargo", "test", "--locked", "--workspace", ACCEPT, "--", "--exact", "--include-ignored"],
    "acceptance_host": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", ACCEPT],
    "host": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", "--", "--test-threads=1"],
    "conversation": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "supervisor_conversation"],
    "central37": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "central37"],
    "format_version": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--lib", "state_format_is_the_latest_migration"],
    "inspect": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "central_sessions"],
    "api": ["cargo", "test", "--locked", "-p", "aq-agent-api", "--lib", "pattern_tests"],
    "format": ["cargo", "fmt", "--check"],
    "clippy": ["cargo", "clippy", "--locked", "-p", "aq-server", "-p", "aq-supervisor-host", "-p", "aq-central-authority", "-p", "aq-agent-api", "--features", "aq-server/test-support", "--all-targets", "--", "-D", "warnings"],
}


def test_cargo():
    check = os.environ.get("F1B4_CHECK", "check")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment.setdefault("CARGO_INCREMENTAL", "0")
    environment.setdefault("RUST_TEST_THREADS", "2")
    environment["PATH"] = str(Path.home() / ".cargo/bin") + os.pathsep + environment.get("PATH", "")
    log = Path(__file__).with_name(f"{check}.txt")
    with log.open("w") as output:
        result = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment,
                                stdout=output, stderr=subprocess.STDOUT)
    assert result.returncode == 0, f"{COMMANDS[check]} exited {result.returncode}; see {log}"
