"""Run the existing Rust store checks under foreground AQ test admission."""
import os
import subprocess
from pathlib import Path


def test_store_migration_prefixes():
    root = Path(__file__).resolve().parents[4]
    evidence = Path(__file__).resolve().parent
    env = dict(os.environ)
    env.update(
        DATABASE_URL="postgres://postgres:test@127.0.0.1:55432/postgres",
        CARGO_TARGET_DIR="/tmp/aq-b3-slot2-target",
        CARGO_BUILD_JOBS="1",
        CARGO_INCREMENTAL="0",
        CARGO_PROFILE_DEV_DEBUG="0",
        RUST_TEST_THREADS="2",
    )
    checks = [
        ("state-format.log", ["--lib", "store::tests::state_format_is_the_latest_migration"]),
        ("store-unit.log", ["--lib", "store::"]),
        ("store-integration.log", ["--test", "store"]),
    ]
    for name, selection in checks:
        command = ["cargo", "test", "--locked", "--offline", "-p", "aq-server", *selection,
                   "--", "--test-threads=2"]
        log = evidence / name
        with log.open("w") as output:
            output.write("Command: " + " ".join(command) + "\n")
            output.flush()
            result = subprocess.run(command, cwd=root, env=env, stdout=output,
                                    stderr=subprocess.STDOUT)
        assert result.returncode == 0, log.read_text()[-16000:]
