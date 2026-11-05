"""Focused supervisor credential Rust checks through AQ's pytest resource gate."""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
ACCEPT = "owner_message_reaches_supervisor_and_reply_is_recorded"
COMMANDS = {
    "credentials": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--lib", "agent_access::messaging::global::reply::tests::"],
    "area": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "central_sessions", "--test", "supervisor_conversation", "--test", "supervisor_turns"],
    "acceptance_host": ["cargo", "test", "--locked", "-p", "aq-supervisor-host", "--test", "supervisor_session", ACCEPT],
    "format_version": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--lib", "state_format_is_the_latest_migration"],
    "image_labels": ["cargo", "test", "--locked", "-p", "aq-server", "--features", "test-support", "--test", "image_labels"],
    "format": ["cargo", "fmt", "--check"],
}


def test_cargo():
    check = os.environ.get("F1B_SCOPE_CHECK", "credentials")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment.setdefault("CARGO_INCREMENTAL", "0")
    environment.setdefault("RUST_TEST_THREADS", "2")
    environment["PATH"] = str(Path.home() / ".cargo/bin") + os.pathsep + environment.get("PATH", "")
    log = Path(__file__).with_name(f"{check}.txt")
    with log.open("w") as output:
        print("$ " + " ".join(COMMANDS[check]), file=output, flush=True)
        result = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment,
                                stdout=output, stderr=subprocess.STDOUT)
        print(f"exit code: {result.returncode}", file=output)
    assert result.returncode == 0, f"{COMMANDS[check]} exited {result.returncode}; see {log}"
