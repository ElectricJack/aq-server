"""Repeatable focused checks, invoked inside aq test's resource admission."""
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[4]
evidence = Path(__file__).resolve().parent
commands = [
    ("fmt-final.log", ["cargo", "fmt", "--check"]),
    ("clippy-after-merge.log", ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"]),
    ("loader-tests.log", ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "dashboard_assets"]),
    ("dashboard-area.log", ["cargo", "test", "--locked", "-p", "aq-server", "--test", "dashboard_assets", "--test", "dashboard_read", "--test", "dashboard_actions", "--test", "dashboard_mounts", "--test", "dashboard_aggregation"]),
]
env = dict(os.environ, CARGO_BUILD_JOBS="2", RUST_TEST_THREADS="2", DATABASE_URL="postgres://postgres:test@127.0.0.1:55432/postgres")
for filename, command in commands:
    print("RUN:", " ".join(command), flush=True)
    with (evidence / filename).open("w") as output:
        result = subprocess.run(command, cwd=root, env=env, stdout=output, stderr=subprocess.STDOUT)
    print("EXIT:", result.returncode, "LOG:", evidence / filename, flush=True)
    if result.returncode:
        print((evidence / filename).read_text()[-7000:], flush=True)
        raise SystemExit(result.returncode)
