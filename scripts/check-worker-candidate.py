#!/usr/bin/env python3
"""Verify the immutable standalone worker source and public contract inputs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def verify_match(actual, expected, message):
    if actual != expected:
        raise ValueError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def source_archive(manifest):
    archive = ROOT / "vendor" / manifest["archive"]
    if archive.parent != ROOT / "vendor" or digest(archive.read_bytes()) != manifest["archive_sha256"]:
        raise ValueError("worker source archive digest mismatch")
    expected = f'{manifest["archive_sha256"]}  {manifest["archive"]}\n'
    verify_match((ROOT / "vendor/worker-candidate.sha256").read_text(), expected, "worker archive checksum file disagrees with its manifest")
    return archive


def package_version(tar, prefix, workspace):
    info = tomllib.loads(tar.extractfile(prefix + "Cargo.toml").read().decode())["package"]
    version = info["version"]
    if isinstance(version, dict) and version.get("workspace"):
        return workspace["package"]["version"]
    return version


def unsafe_path(name):
    path = Path(name)
    return path.is_absolute() or ".." in path.parts


def check_members(tar):
    if any(unsafe_path(name) for name in tar.getnames()):
        raise ValueError("unsafe source archive member")
    for member in tar.getmembers():
        check_member_type(member)


def check_member_type(member):
    if member.issym() or member.islnk():
        if unsafe_path(member.linkname):
            raise ValueError("unsafe source archive link")
    elif not (member.isfile() or member.isdir()):
        raise ValueError("unsupported source archive member")


def check_package_source(tar, package, base, prefix):
    # Cargo generates these files during packaging. Cargo.toml.orig retains the original manifest.
    provided = {".cargo_vcs_info.json", "Cargo.lock", "Cargo.toml"}
    for item in package["files"]:
        path = item["path"]
        if path in provided:
            continue
        source_path = "Cargo.toml" if path == "Cargo.toml.orig" else path
        verify_match(tar.extractfile(prefix + source_path).read(), (base / path).read_bytes(), f'worker and daemon public package disagree: {package["name"]}/{source_path}')


def check_public_package(tar, package, workspace):
    base = ROOT / "vendor" / f'{package["name"]}-{package["version"]}'
    for item in package["files"]:
        verify_match(digest((base / item["path"]).read_bytes()), item["sha256"], f'public contract changed: {package["name"]}/{item["path"]}')
    prefix = f'crates/{package["name"]}/'
    check_package_source(tar, package, base, prefix)
    check_source_set(tar, package, prefix)
    verify_match(package_version(tar, prefix, workspace), package["version"], f'worker and daemon contract versions disagree: {package["name"]}')


def check_source_set(tar, package, prefix):
    expected_sources = {item["path"] for item in package["files"] if item["path"].startswith("src/")}
    actual_sources = {member.name[len(prefix):] for member in tar.getmembers()
                      if member.isfile() and member.name.startswith(prefix + "src/")}
    verify_match(actual_sources, expected_sources, f'worker and daemon contract source sets disagree: {package["name"]}')


def verify_archive(archive, manifest, contracts):
    with tarfile.open(archive) as tar:
        workspace = tomllib.loads(tar.extractfile("Cargo.toml").read().decode())["workspace"]
        verify_match(tar.pax_headers.get("comment"), manifest["source_revision"], "source archive commit identity disagrees with candidate")
        verify_match(package_version(tar, "crates/aq-worker/", workspace), manifest["version"], "worker package version disagrees with candidate")
        check_members(tar)
        for package in contracts["packages"]:
            check_public_package(tar, package, workspace)
        lock = tar.extractfile("Cargo.lock").read()
        verify_match(digest(lock), manifest["lockfile_sha256"], "worker lockfile identity disagrees with candidate")


def candidate(official=False):
    if official and any(os.environ.get(k) for k in ("AQ_WORKER_BIN", "AQ_WORKER_SOURCE_DIR")):
        raise ValueError("official builds reject local worker overrides")
    manifest = json.loads((ROOT / "vendor/worker-candidate.json").read_text())
    if not re.fullmatch(r"[0-9a-f]{40}", manifest["source_revision"]):
        raise ValueError("worker source must name a full immutable commit")
    archive = source_archive(manifest)
    contracts = json.loads((ROOT / "vendor/worker-contracts.json").read_text())
    verify_archive(archive, manifest, contracts)
    return manifest


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--official", action="store_true")
    parser.add_argument("--revision", action="store_true")
    args = parser.parse_args()
    try:
        value = candidate(args.official)
    except (ValueError, KeyError, OSError, tarfile.TarError) as error:
        parser.exit(1, f"worker candidate: {error}\n")
    print(value["source_revision"] if args.revision else json.dumps(value, indent=2))
