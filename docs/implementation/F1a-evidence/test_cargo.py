"""Focused Rust checks through AQ's pytest resource gate."""
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
COMMANDS = {
    "shutdown_signals": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--bin", "aq-supervisor-host", "shutdown_receives_sigterm_and_sigint"],
    "merged_start": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", "supervisor_session_starts_under_central_permit"],
    "merged_restart": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", "supervisor_restart_resumes_without_duplicate_session"],
    "upgrade": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", "main_delivery_database_upgrades_without_losing_history"],
    "start_revocation_red": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", "revocation_before_physical_start"],
    "image": [sys.executable, "docs/implementation/F1a-evidence/image_check.py"],
    "host_all": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--", "--test-threads=1"],
    "named_start": ["cargo", "test", "--locked", "--workspace", "supervisor_session_starts_under_central_permit"],
    "named_restart": ["cargo", "test", "--locked", "--workspace", "supervisor_restart_resumes_without_duplicate_session"],
    "host_clippy": ["cargo", "clippy", "--locked", "-p", "aq-supervisor-host", "--all-targets", "--", "-D", "warnings"],
    "adapter_red": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "adapter"],
    "adapter": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "adapter"],
    "credentials": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "credentials"],
    "credentials_red": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "credentials"],
    "host": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", "--", "--test-threads=1"],
    "host_red": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session"],
    "owner": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "central_sessions", "--", "--test-threads=1"],
    "model": ["cargo", "test", "--locked", "-p", "aq-central-authority"],
    "format": ["cargo", "fmt", "--check"],
    "clippy": ["cargo", "clippy", "--locked", "-p", "aq-central-authority", "-p", "aq-server", "--features", "test-support", "--lib", "--test", "central_sessions", "--", "-D", "warnings"],
    "format_version": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--lib", "state_format_is_the_latest_migration"],
}


def test_cargo():
    check = os.environ.get("F1A_CHECK", "owner")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment.setdefault("CARGO_INCREMENTAL", "0")
    environment.setdefault("RUST_TEST_THREADS", "2")
    log = Path(__file__).with_name(f"{check}.txt")
    with log.open("w") as output:
        result = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment,
                                stdout=output, stderr=subprocess.STDOUT)
    assert result.returncode == 0, f"{COMMANDS[check]} exited {result.returncode}; see {log}"
