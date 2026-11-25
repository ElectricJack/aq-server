#!/usr/bin/env python3
"""Verify the retained AQ08 pure contract archives without sibling checkouts."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

from qualification_archives import CrateArchive, patch_dependencies

ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = ROOT / "docs/implementation/evidence/aq08-policy-4b7cc26"
QUALIFICATION = ROOT / "docs/implementation/AQ-08-acyclic-graph-policy-qualification.json"
PACKAGES = {
    "aq-agent-api": "0.1.0",
    "aq-session-api": "0.1.0",
    "aq-session-protocol": "0.1.0",
    "aq-profiles": "0.1.1",
    "aq-selection": "0.1.2",
}


def qualify(work, manifest, environment):
    commands = [
        ["cargo", "test", "--manifest-path", str(manifest), "--locked", "--offline"],
        ["cargo", "clippy", "--manifest-path", str(manifest), "--all-targets", "--all-features", "--locked", "--offline", "--", "-D", "warnings"],
        ["cargo", "fmt", "--manifest-path", str(manifest), "--check"],
    ]
    for command in commands:
        print("COMMAND", json.dumps(command), flush=True)
        subprocess.run(command, cwd=work, env=environment, check=True)


def main():
    expected = json.loads(QUALIFICATION.read_text())["archives_sha256"]
    work = Path(tempfile.mkdtemp(prefix="aq08-pure-packages-"))
    packages = {}
    for name, version in PACKAGES.items():
        directory = f"{name}-{version}"
        filename = directory + ".crate"
        archive = CrateArchive(EVIDENCE / "archives" / filename, directory, expected[filename], filename)
        packages[name] = archive.extract(work)
    # Selection is a root package; patching it as an unused dependency changes its lockfile.
    patch_dependencies(work, packages, "aq-selection")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
    environment["CARGO_INCREMENTAL"] = "0"
    for name in ("aq-profiles", "aq-selection"):
        qualify(work, packages[name] / "Cargo.toml", environment)
    print(f"Verified extracted archive closure in {work}", flush=True)


if __name__ == "__main__":
    main()
