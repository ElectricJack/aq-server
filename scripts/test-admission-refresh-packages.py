#!/usr/bin/env python3
"""Qualify the frozen registry-refresh admission archive without a live sibling checkout."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from qualification_archives import extract_inventory, patch_dependencies

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--inventory", type=Path, default=ROOT / "protocol/accepted-preparation-refresh/v1/packages.json")
    arguments = parser.parse_args()
    if not os.environ.get("CARGO_TARGET_DIR"):
        raise SystemExit("CARGO_TARGET_DIR must name the explicitly leased existing target")
    inventory = json.loads(arguments.inventory.read_text())
    work = Path(tempfile.mkdtemp(prefix="aq-admission-refresh-closure-"))
    packages = extract_inventory(ROOT, inventory, work)
    patch_dependencies(work, packages, "aq-admission")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment["CARGO_INCREMENTAL"] = "0"
    manifest = packages["aq-admission"] / "Cargo.toml"
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
