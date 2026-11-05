"""Focused wise-apex-23 Rust checks through AQ's pytest resource gate."""
import os
from pathlib import Path
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[3]
FEATURES = ["--features", "test-support"]
COMMANDS = {
    "fmt": ["cargo", "fmt", "-p", "aq-server", "--", "--check"],
    "turns": ["cargo", "test", "--locked", "-p", "aq-server", *FEATURES, "--test", "supervisor_turns"],
    "conversation": ["cargo", "test", "--locked", "-p", "aq-server", *FEATURES, "--test", "supervisor_conversation"],
    "reply_lib": ["cargo", "test", "--locked", "-p", "aq-server", *FEATURES, "--lib", "messaging::global"],
    "format_version": ["cargo", "test", "--locked", "-p", "aq-server", *FEATURES, "--lib", "state_format_is_the_latest_migration"],
    "clippy": ["cargo", "clippy", "--locked", "-p", "aq-server", *FEATURES, "--all-targets", "--", "-D", "warnings"],
}
SELECTED = os.environ.get("CHECKS", ",".join(COMMANDS)).split(",")


@pytest.mark.parametrize("check", SELECTED)
def test_cargo(check):
    environment = os.environ.copy()
    for key, value in {"CARGO_BUILD_JOBS": "2", "CARGO_PROFILE_DEV_DEBUG": "0",
                       "CARGO_INCREMENTAL": "0", "RUST_TEST_THREADS": "2"}.items():
        environment.setdefault(key, value)
    environment["PATH"] = str(Path.home() / ".cargo/bin") + os.pathsep + environment.get("PATH", "")
    log = Path(__file__).with_name(f"{check}.txt")
    with log.open("w") as output:
        result = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment,
                                stdout=output, stderr=subprocess.STDOUT)
    assert result.returncode == 0, f"{COMMANDS[check]} exited {result.returncode}; see {log}"
