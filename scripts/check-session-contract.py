#!/usr/bin/env python3
"""Verify the pinned session contract package without a sibling checkout."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def verify_match(actual, expected, message):
    if actual != expected:
        raise ValueError(message)


def check(root=ROOT):
    vendor = root / "vendor"
    manifest = json.loads((vendor / "session-contract.json").read_text())
    prefix = package_prefix(manifest)
    archive = vendor / manifest["archive"]
    verify_match(sha256(archive.read_bytes()), manifest["archive_sha256"], "session contract archive digest mismatch")
    expected = expected_inventory(manifest)
    packaged = read_archive(archive, prefix)
    verify_match(set(packaged), set(expected), "session contract archive inventory disagrees")
    check_local(vendor / prefix, packaged, expected)
    check_provenance(packaged, manifest)
    return manifest


def package_prefix(manifest):
    revision = manifest["source_revision"]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("session contract source revision is not immutable")
    name, version = manifest["package"], manifest["version"]
    if name != "aq-session-protocol" or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("unexpected session contract package")
    prefix = f"{name}-{version}"
    verify_match(manifest["archive"], prefix + ".crate", "unexpected session contract archive path")
    return prefix


def expected_inventory(manifest):
    expected = {item["path"]: item["sha256"] for item in manifest["files"]}
    if not expected:
        raise ValueError("duplicate or empty session contract inventory")
    verify_match(len(expected), len(manifest["files"]), "duplicate or empty session contract inventory")
    return expected


def read_archive(archive, prefix):
    packaged = {}
    with tarfile.open(archive) as tar:
        for member in tar:
            relative = member_path(member, prefix)
            if relative in packaged:
                raise ValueError("duplicate session contract archive member")
            packaged[relative] = tar.extractfile(member).read()
    return packaged


def member_path(member, prefix):
    path = PurePosixPath(member.name)
    if not member.isfile() or ".." in path.parts:
        raise ValueError("unsafe session contract archive member")
    if len(path.parts) < 2 or path.parts[0] != prefix:
        raise ValueError("unsafe session contract archive member")
    return str(PurePosixPath(*path.parts[1:]))


def check_local(local, packaged, expected):
    verify_match(local_inventory(local), set(expected), "vendored session contract inventory disagrees")
    for relative, content in packaged.items():
        message = f"session contract content drift: {relative}"
        verify_match(sha256(content), expected[relative], message)
        verify_match((local / relative).read_bytes(), content, message)


def local_inventory(local):
    local_paths = set()
    for path in local.rglob("*"):
        if path.is_symlink():
            raise ValueError("session contract package contains a symlink")
        if path.is_file():
            local_paths.add(path.relative_to(local).as_posix())
    return local_paths


def check_provenance(packaged, manifest):
    git = json.loads(packaged[".cargo_vcs_info.json"])["git"]
    verify_match((git["sha1"], bool(git.get("dirty"))), (manifest["source_revision"], False),
                 "session contract was not packaged from the selected clean revision")
    package = tomllib.loads(packaged["Cargo.toml"].decode())["package"]
    verify_match((package["name"], package["version"]), (manifest["package"], manifest["version"]),
                 "session contract package metadata disagrees")


if __name__ == "__main__":
    try:
        manifest = check()
    except (KeyError, OSError, ValueError, tarfile.TarError) as error:
        raise SystemExit(f"session contract: {error}")
    print(f"PASS: {manifest['package']} {manifest['version']} at {manifest['source_revision']}")
