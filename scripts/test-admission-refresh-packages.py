#!/usr/bin/env python3
"""Qualify the frozen registry-refresh admission archive without a live sibling checkout."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--inventory", type=Path, default=ROOT / "protocol/accepted-preparation-refresh/v1/packages.json")
    arguments = parser.parse_args()
    if not os.environ.get("CARGO_TARGET_DIR"):
        raise SystemExit("CARGO_TARGET_DIR must name the explicitly leased existing target")
    inventory = json.loads(arguments.inventory.read_text())
    work = Path(tempfile.mkdtemp(prefix="aq-admission-refresh-closure-"))
    packages = {}
    for package in inventory["packages"]:
        name, version = package["name"], package["version"]
        archive = ROOT / package["archive"]
        if hashlib.sha256(archive.read_bytes()).hexdigest() != package["sha256"]:
            raise SystemExit(f"archive digest mismatch: {name}-{version}")
        with tarfile.open(archive) as contents:
            prefix = f"{name}-{version}/"
            if any(not member.name.startswith(prefix) for member in contents.getmembers()):
                raise SystemExit(f"invalid archive root: {name}-{version}")
            contents.extractall(work, filter="data")
        packages[name] = (version, work / f"{name}-{version}")
    (work / ".cargo").mkdir()
    (work / ".cargo/config.toml").write_text(
        "[patch.crates-io]\n" + "".join(
            f"{name} = {{ path = {json.dumps(str(directory))} }}\n"
            for name, (_version, directory) in packages.items() if name != "aq-admission"
        )
    )
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment["CARGO_INCREMENTAL"] = "0"
    for name in ["aq-admission"]:
        manifest = packages[name][1] / "Cargo.toml"
        for command in [
            ["cargo", "test", "--manifest-path", str(manifest), "--locked", "--offline"],
            ["cargo", "clippy", "--manifest-path", str(manifest), "--all-targets", "--locked", "--offline", "--", "-D", "warnings"],
            ["cargo", "fmt", "--manifest-path", str(manifest), "--check"],
        ]:
            print("COMMAND", json.dumps(command), flush=True)
            subprocess.run(command, cwd=work, env=environment, check=True)
    print(f"Qualified exact extracted admission refresh closure at {work}", flush=True)


if __name__ == "__main__":
    main()
