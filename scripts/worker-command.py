#!/usr/bin/env python3
"""Run the pinned standalone connector, or an explicitly selected local binary."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("worker_candidate", ROOT / "scripts/check-worker-candidate.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)

override = os.environ.get("AQ_WORKER_BIN")
if override:
    executable = Path(override).resolve()
    print(f"worker: explicit development executable override {executable}", file=sys.stderr)
    sys.exit(subprocess.call([str(executable), *sys.argv[1:]]))

try:
    candidate = checker.candidate()
except (ValueError, OSError, KeyError) as error:
    sys.exit(f"worker candidate: {error}")
print(f'worker: pinned standalone source {candidate["source_revision"]}', file=sys.stderr)
with tempfile.TemporaryDirectory(prefix="aq-worker-source-") as directory:
    with tarfile.open(ROOT / "vendor" / candidate["archive"]) as archive:
        archive.extractall(directory, filter="data")
    env = os.environ.copy()
    env["CARGO_INCREMENTAL"] = "0"
    env["CARGO_TARGET_DIR"] = str(ROOT / "state.dev/worker-target" / candidate["archive_sha256"])
    env["AQ_SOURCE_REVISION"] = candidate["source_revision"]
    sys.exit(subprocess.call([
        "cargo", "run", "--locked", "--manifest-path", str(Path(directory) / "Cargo.toml"),
        "-p", "aq-worker", "--bin", "aq-worker", "--", *sys.argv[1:]
    ], env=env))
