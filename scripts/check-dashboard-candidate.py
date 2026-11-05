#!/usr/bin/env python3
"""Verify the published dashboard pin, bounded archive, and server-image wiring."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile

ROOT = Path(__file__).resolve().parents[1]
MAX_TOTAL = 32 * 1024 * 1024
MAX_MANIFEST = 256 * 1024
ASSET = re.compile(r"assets/[A-Za-z0-9_.-]+\.(js|css|woff2?|png|svg|jpg|webp|ico)\Z")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def verify(root=ROOT):
    vendor = root / "vendor"
    pin = json.loads((vendor / "dashboard-candidate.json").read_bytes())
    require(pin["component"] == "aq-dashboard", "wrong dashboard component")
    archive = f"aq-dashboard-{pin['tag']}.tar.gz"
    require(pin["archive"] == archive and pin["tag"] == f"v{pin['version']}", "release tag/archive mismatch")
    require(pin["asset_url"] == f"https://github.com/ElectricJack/aq-dashboard/releases/download/{pin['tag']}/{archive}", "release URL mismatch")
    require(re.fullmatch(r"[a-f0-9]{64}", pin["archive_sha256"]), "invalid archive digest")
    require(re.fullmatch(r"[a-f0-9]{40}", pin["source_revision"]), "invalid source revision")
    record = json.loads((vendor / f"{archive}.release.json").read_bytes())
    require(pin == dict(component="aq-dashboard", archive=archive, **record), "release record differs from pin")
    require(record["schema_version"] == 1 and record["manifest_schema_version"] == 2, "unsupported release schema")
    require((vendor / f"{archive}.sha256").read_text().split() == [pin["archive_sha256"], archive], "checksum sidecar differs from pin")
    archive_path = vendor / archive
    require(archive_path.stat().st_size <= MAX_TOTAL + MAX_MANIFEST, "archive byte limit")
    require(sha256(archive_path.read_bytes()) == pin["archive_sha256"], "archive SHA256 mismatch")
    files = {}
    with tarfile.open(archive_path, "r:gz") as bundle:
        total = 0
        for member in bundle:
            require(len(files) < 513, "archive file count limit")
            require(member.isfile() and not member.issym() and not member.islnk(), "archive member must be a regular file")
            name = member.name
            require(name not in files, "duplicate archive member")
            require(name in ("index.html", "dashboard-manifest.json") or (ASSET.fullmatch(name) and len(name.removeprefix("assets/")) <= 240), "unsafe archive member path/type")
            limit = MAX_MANIFEST if name == "dashboard-manifest.json" else MAX_TOTAL
            require(0 <= member.size <= limit, "archive member byte limit")
            total += member.size
            require(total <= MAX_TOTAL + MAX_MANIFEST, "archive total byte limit")
            with bundle.extractfile(member) as stream:
                data = stream.read(limit + 1)
            require(len(data) == member.size, "archive member size mismatch")
            files[name] = data
    manifest_bytes = files.pop("dashboard-manifest.json")
    require(f"sha256:{sha256(manifest_bytes)}" == pin["dashboard_digest"], "manifest SHA256 mismatch")
    manifest = json.loads(manifest_bytes)
    require(manifest["schema_version"] == pin["manifest_schema_version"], "manifest schema mismatch")
    require(manifest["version"] == pin["version"], "manifest version mismatch")
    require(manifest["source"] == {"revision": pin["source_revision"], "dirty": False, "official": True}, "manifest provenance mismatch")
    require(set(files) == set(manifest["files"]) and "index.html" in files, "archive inventory mismatch")
    require(len(files) <= 512 and sum(map(len, files.values())) <= MAX_TOTAL, "dashboard bundle limits")
    for name, data in files.items():
        require(manifest["files"][name] == {"sha256": sha256(data), "bytes": len(data)}, f"asset integrity mismatch: {name}")
    docker = (root / "Dockerfile").read_text().split("FROM runtime AS server\n", 1)[1]
    wiring = (
        f"COPY vendor/{archive} /tmp/dashboard.tar.gz\n"
        f"RUN echo '{pin['archive_sha256']}  /tmp/dashboard.tar.gz' | sha256sum -c - \\\n"
        "    && mkdir -p /usr/share/agent-q/dashboard \\\n"
        "    && tar -xzf /tmp/dashboard.tar.gz -C /usr/share/agent-q/dashboard \\\n"
        "    && rm /tmp/dashboard.tar.gz\n"
    )
    require(wiring in docker, "Docker server stage archive verification/extraction differs from pin")
    require("ENV AQ_DASHBOARD_DIR=/usr/share/agent-q/dashboard\n" in docker, "Docker dashboard directory missing")
    require(docker.index(wiring) < docker.index("USER 10001:10001"), "dashboard must be installed before dropping privileges")
    files["dashboard-manifest.json"] = manifest_bytes
    return pin, files


def extract(files, destination):
    require(destination.is_dir() and not destination.is_symlink() and not any(destination.iterdir()), "extraction destination must be an empty real directory")
    (destination / "assets").mkdir(mode=0o755)
    for name, data in files.items():
        # All paths/types were verified above; never use tar's link extraction.
        with (destination / name).open("xb") as output:
            output.write(data)
        (destination / name).chmod(0o644)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extract", type=Path, help="extract verified bundle into an existing empty directory")
    args = parser.parse_args()
    pin, files = verify()
    if args.extract:
        extract(files, args.extract)
    print(json.dumps({"tag": pin["tag"], "archive_sha256": pin["archive_sha256"], "dashboard_digest": pin["dashboard_digest"], "files": len(files) - 1}))


if __name__ == "__main__":
    main()
