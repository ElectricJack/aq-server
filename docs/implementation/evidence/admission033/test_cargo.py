"""Focused admission 0.3.3 pin checks through AQ resource admission."""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[4]
COMMANDS = [
    ["cargo", "fmt", "-p", "aq-server", "--check"],
    ["cargo", "test", "--locked", "--offline", "-p", "aq-server", "--test", "permit_fixture_conformance"],
    ["cargo", "test", "--locked", "--offline", "-p", "aq-admission@0.3.3", "--test", "sandbox"],
    ["cargo", "test", "--locked", "--offline", "-p", "aq-admission@0.1.11", "--test", "transport"],
]


def test_admission033():
    env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target"),
               CARGO_INCREMENTAL="0", CARGO_BUILD_JOBS="2", RUST_TEST_THREADS="2")
    log = Path(__file__).with_name("checks.txt")
    with log.open("w") as output:
        for command in COMMANDS:
            output.write("$ " + " ".join(command) + "\n")
            output.flush()
            result = subprocess.run(command, cwd=ROOT, env=env,
                                    stdout=output, stderr=subprocess.STDOUT)
            assert result.returncode == 0, f"{command} exited {result.returncode}; see {log}"
