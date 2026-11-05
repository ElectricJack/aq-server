#!/usr/bin/env python3
"""Verify the retained AQ08 pure contract archives without sibling checkouts."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

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


def main():
    expected = json.loads(QUALIFICATION.read_text())["archives_sha256"]
    work = Path(tempfile.mkdtemp(prefix="aq08-pure-packages-"))
    for name, version in PACKAGES.items():
        filename = f"{name}-{version}.crate"
        archive = EVIDENCE / "archives" / filename
        if hashlib.sha256(archive.read_bytes()).hexdigest() != expected[filename]:
            raise SystemExit(f"archive digest mismatch: {filename}")
        with tarfile.open(archive) as contents:
            prefix = f"{name}-{version}/"
            if any(not member.name.startswith(prefix) for member in contents.getmembers()):
                raise SystemExit(f"invalid archive root: {filename}")
            contents.extractall(work, filter="data")
    (work / ".cargo").mkdir()
    # Selection is a root package; patching it as an unused dependency would
    # change the packaged profile lockfile. All real AQ dependencies are here.
    patches = {name: version for name, version in PACKAGES.items() if name != "aq-selection"}
    (work / ".cargo/config.toml").write_text(
        "[patch.crates-io]\n"
        + "".join(
            f"{name} = {{ path = {json.dumps(str(work / f'{name}-{version}'))} }}\n"
            for name, version in patches.items()
        )
    )
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
    environment["CARGO_INCREMENTAL"] = "0"
    for name in ("aq-profiles", "aq-selection"):
        manifest = work / f"{name}-{PACKAGES[name]}" / "Cargo.toml"
        commands = [
            ["cargo", "test", "--manifest-path", str(manifest), "--locked", "--offline"],
            ["cargo", "clippy", "--manifest-path", str(manifest), "--all-targets", "--all-features", "--locked", "--offline", "--", "-D", "warnings"],
            ["cargo", "fmt", "--manifest-path", str(manifest), "--check"],
        ]
        for command in commands:
            print("COMMAND", json.dumps(command), flush=True)
            subprocess.run(command, cwd=work, env=environment, check=True)
    print(f"Verified extracted archive closure in {work}", flush=True)


if __name__ == "__main__":
    main()
