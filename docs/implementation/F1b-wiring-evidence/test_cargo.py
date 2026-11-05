"""Focused supervisor binary wiring checks through AQ's resource gate."""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
COMMANDS = {
    "focused": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--lib", "--bin", "aq-supervisor-host", "--test", "adapter", "--test", "credentials"],
    "acceptance": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", "owner_message_reaches_supervisor_and_reply_is_recorded", "--", "--exact"],
    "clippy": ["cargo", "clippy", "--locked", "-p", "aq-supervisor-host", "--all-targets", "--", "-D", "warnings"],
    "format": ["cargo", "fmt", "--package", "aq-supervisor-host", "--check"],
}


def test_cargo():
    check = os.environ["F1B_WIRING_CHECK"]
    environment = os.environ.copy()
    environment["CARGO_BUILD_JOBS"] = "2"
    environment["CARGO_PROFILE_DEV_DEBUG"] = "0"
    environment["CARGO_PROFILE_TEST_DEBUG"] = "0"
    environment["CARGO_INCREMENTAL"] = "0"
    environment["RUST_TEST_THREADS"] = "2"
    environment["PATH"] = str(Path.home() / ".cargo/bin") + os.pathsep + environment.get("PATH", "")
    log = Path(__file__).with_name(f"{check}.txt")
    with log.open("w") as output:
        completed = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment,
                                   stdout=output, stderr=subprocess.STDOUT)
    assert completed.returncode == 0, f"{COMMANDS[check]} exited {completed.returncode}; see {log}"
