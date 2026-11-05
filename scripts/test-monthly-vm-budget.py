"""Focused monthly VM accounting checks under aq test's foreground slot."""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def test_monthly_vm_budget():
    environment = os.environ.copy()
    environment["CARGO_BUILD_JOBS"] = "2"
    environment["CARGO_INCREMENTAL"] = "0"
    environment["RUST_TEST_THREADS"] = "1"
    assert environment.get("DATABASE_URL"), "set DATABASE_URL via scripts/test-db"
    commands = [
        ["cargo", "test", "--locked", "-p", "aq-accounting"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "observations::vm_time"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "profiled_execution::placement"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--test", "observations", "monthly_vm_budget"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--test", "observations", "accounting::"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--test", "image_labels"],
        ["cargo", "test", "--locked", "-p", "aq-server", "--test", "store"],
    ]
    for command in commands:
        print("COMMAND", " ".join(command), flush=True)
        subprocess.run(command, cwd=ROOT, env=environment, check=True)
