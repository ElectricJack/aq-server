#!/usr/bin/env python3
"""Build the exact pending merged revision for the disposable deployment environment."""

import argparse
from contextlib import ExitStack, closing
from fcntl import LOCK_EX, flock
from hashlib import file_digest
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import tarfile
from tempfile import TemporaryDirectory

SOURCE = "https://github.com/ElectricJack/aq-graph"
ENVIRONMENT = "aq-graph-deploy-canary"


def git(repo, *args):
    return subprocess.check_output(
        ["git", "--no-replace-objects", "-C", str(repo), *args], text=True,
    ).strip()


def docker(directory, *args, **options):
    return subprocess.check_output(
        ["docker", "--config", str(directory / "docker"),
         "--host", "unix:///var/run/docker.sock", *args],
        timeout=options.pop("timeout", 30), **options,
    )


def digest(path):
    with path.open("rb") as stream:
        return "sha256:" + file_digest(stream, "sha256").hexdigest()


def private(path, text):
    path.touch(mode=0o600, exist_ok=False)
    path.write_text(text)


def history(repo):
    if git(repo, "remote", "get-url", "origin").removesuffix(".git") != SOURCE:
        raise ValueError("expected the aq-graph origin")
    if git(repo, "rev-parse", "--is-shallow-repository") != "false":
        raise ValueError("an unshallowed aq-graph history is required to order merged revisions")
    return git(repo, "rev-list", "--first-parent", "refs/remotes/origin/main").splitlines()


def build_image(directory, repo, revision, instance, cleanup):
    archive = directory / f"{revision}.source.tar"
    iid = directory / f"{revision}.id"
    tag = f"{ENVIRONMENT}-{instance}:{revision}"
    with archive.open("wb") as stream:
        subprocess.run(
            ["git", "--no-replace-objects", "-C", str(repo), "archive", "--format=tar", revision],
            stdout=stream, check=True, timeout=30,
        )
    with archive.open("rb") as stream:
        docker(
            directory, "buildx", "build", "--builder", "default", "--load", "--platform", "linux/amd64",
            "--build-arg", "CARGO_PROFILE=ci-image", "--iidfile", str(iid),
            "--label", f"org.opencontainers.image.revision={revision}",
            "--label", f"org.opencontainers.image.source={SOURCE}", "-t", tag, "-",
            stdin=stream, timeout=900,
        )
    cleanup.callback(docker, directory, "image", "rm", tag)
    return archive, iid.read_text().strip()


def export_image(directory, revision, image_id):
    output = directory / "provenance" / f"{revision}.tar"
    docker(directory, "image", "save", "-o", str(output), image_id, timeout=90)
    with tarfile.open(output) as layout, layout.extractfile("index.json") as stream:
        manifest, = json.load(stream)["manifests"]
    return output, manifest["digest"]


def build(directory, repo, revision, instance, cleanup):
    revisions = history(repo)
    if revision not in revisions:
        raise ValueError(f"revision {revision} is absent from aq-graph origin/main first-parent history")
    print(f"Building aq-graph {revision}", flush=True)
    archive, image_id = build_image(directory, repo, revision, instance, cleanup)
    output, image_digest = export_image(directory, revision, image_id)
    evidence = {
        "source": SOURCE, "revision": revision, "image_id": image_id,
        "image_digest": image_digest, "source_archive_digest": digest(archive),
        "image_archive_digest": digest(output), "source_position": len(revisions) - revisions.index(revision),
    }
    (directory / "provenance" / f"{revision}.json").write_text(json.dumps(evidence))
    print(json.dumps(evidence, sort_keys=True), flush=True)


def build_next(instance_dir, repo):
    settings = json.loads((instance_dir / "environment.json").read_text())
    instance = settings["instance"]
    if settings["environment"] != ENVIRONMENT or not re.fullmatch(r"[0-9a-f]{12}", instance):
        raise ValueError("expected a disposable aq-graph deployment instance")
    database = instance_dir / "deployments.sqlite3"
    if not database.is_file():
        return
    with (instance_dir / "builder.lock").open("a") as lock:
        flock(lock, LOCK_EX)
        with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as db:
            pending = db.execute("""
                SELECT revision FROM merge_claim WHERE resolution='pending'
                ORDER BY observed_at_ms, operation_id LIMIT 1
            """).fetchone()
        if not pending:
            return
        revision, = pending
        destination = instance_dir / "provenance"
        destination.mkdir(mode=0o700, exist_ok=True)
        if (destination / f"{revision}.json").exists():
            return
        publish_build(instance_dir, repo, revision, instance, destination)


def publish_build(instance_dir, repo, revision, instance, destination):
    with TemporaryDirectory(dir=instance_dir) as work, ExitStack() as cleanup:
        build_dir = Path(work)
        (build_dir / "provenance").mkdir()
        (build_dir / "docker").mkdir()
        (build_dir / "docker/config.json").write_text("{}\n")
        build(build_dir, repo, revision, instance, cleanup)
        # The controller can read the receipt as soon as the archive is available.
        for suffix in ("tar", "json"):
            name = f"{revision}.{suffix}"
            os.replace(build_dir / "provenance" / name, destination / name)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("instance_directory", type=Path)
    parser.add_argument("graph_repo", type=Path)
    args = parser.parse_args()
    build_next(args.instance_directory.resolve(), args.graph_repo)
