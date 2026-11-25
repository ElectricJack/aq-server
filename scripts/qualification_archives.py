"""Verify and extract the exact crate archives used by the qualification scripts."""
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import tarfile


@dataclass(frozen=True)
class CrateArchive:
    path: Path
    directory: str
    sha256: str
    label: str

    def extract(self, work):
        if hashlib.sha256(self.path.read_bytes()).hexdigest() != self.sha256:
            raise SystemExit(f"archive digest mismatch: {self.label}")
        with tarfile.open(self.path) as contents:
            prefix = self.directory + "/"
            if any(not member.name.startswith(prefix) for member in contents.getmembers()):
                raise SystemExit(f"invalid archive root: {self.label}")
            contents.extractall(work, filter="data")
        return work / self.directory


def extract_inventory(root, inventory, work):
    packages = {}
    for package in inventory["packages"]:
        name = package["name"]
        directory = f"{name}-{package['version']}"
        archive = CrateArchive(root / package["archive"], directory, package["sha256"], directory)
        packages[name] = archive.extract(work)
    return packages


def patch_dependencies(work, packages, root_package):
    cargo = work / ".cargo"
    cargo.mkdir()
    patches = (
        f"{name} = {{ path = {json.dumps(str(directory))} }}\n"
        for name, directory in packages.items() if name != root_package
    )
    (cargo / "config.toml").write_text("[patch.crates-io]\n" + "".join(patches))
