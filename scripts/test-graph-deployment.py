#!/usr/bin/env python3
"""Build two merged aq-graph revisions and qualify the Host11 deployment playbook."""

import argparse
from contextlib import ExitStack
from hashlib import file_digest
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tarfile
from tempfile import TemporaryDirectory
import time
from uuid import uuid4

ROOT = Path(__file__).resolve().parents[1]
SOURCE = "https://github.com/ElectricJack/aq-graph"
ENVIRONMENT = "aq-graph-deploy-canary"
OWNER = "io.aq.graph-deploy.environment"
TEST = "real_adapter_pilots::deployment_canary::merged_graph_rolls_out_by_verified_digest"


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


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


def build(directory, repo, revision, instance, cleanup):
    print(f"Building aq-graph {revision}", flush=True)
    archive = directory / f"{revision}.source.tar"
    iid = directory / f"{revision}.id"
    tag = f"{ENVIRONMENT}-{instance}:{revision}"
    with archive.open("wb") as stream:
        subprocess.run(
            ["git", "-C", str(repo), "archive", "--format=tar", revision],
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
    image_id = iid.read_text().strip()
    output = directory / "provenance" / f"{revision}.tar"
    docker(directory, "image", "save", "-o", str(output), image_id, timeout=90)
    with tarfile.open(output) as layout, layout.extractfile("index.json") as stream:
        manifest, = json.load(stream)["manifests"]
    evidence = {
        "source": SOURCE, "revision": revision, "image_id": image_id,
        "image_digest": manifest["digest"], "source_archive_digest": digest(archive),
        "image_archive_digest": digest(output),
    }
    (directory / "provenance" / f"{revision}.json").write_text(json.dumps(evidence))
    print(json.dumps(evidence, sort_keys=True), flush=True)


def provision(directory, instance, resources):
    prefix = f"{ENVIRONMENT}-{instance}"
    network, database = f"{prefix}-network", f"{prefix}-postgres"
    label = f"{OWNER}={instance}"
    password = secrets.token_hex(24)
    docker(directory, "network", "create", "--label", label, network)
    resources.callback(docker, directory, "network", "rm", network)
    postgres_env = directory / "postgres.env"
    private(postgres_env, f"POSTGRES_PASSWORD={password}\nPOSTGRES_DB=graph\n")
    container_id = docker(
        directory, "run", "-d", "--pull=never", "--name", database, "--label", label,
        "--network", network, "-p", "127.0.0.1::5432", "--tmpfs", "/var/lib/postgresql/data",
        "--env-file", str(postgres_env), "postgres:17-bookworm",
    ).decode().strip()
    resources.callback(docker, directory, "rm", "-fv", container_id)
    resources.callback(remove_graph, directory, instance)
    private(directory / "graph.env", f"DATABASE_URL=postgres://postgres:{password}@{database}/graph\n")
    for _ in range(100):
        try:
            docker(directory, "exec", database, "pg_isready", "-h", "127.0.0.1", "-U", "postgres")
            break
        except subprocess.CalledProcessError:
            time.sleep(0.2)
    else:
        raise RuntimeError("disposable PostgreSQL did not become ready")
    address = docker(directory, "port", database, "5432/tcp").decode().strip()
    if not re.fullmatch(r"127\.0\.0\.1:[0-9]+", address):
        raise RuntimeError("expected one loopback PostgreSQL address")
    return f"postgres://postgres:{password}@{address}/postgres"


def remove_graph(directory, instance):
    ids = docker(
        directory, "ps", "-aq", "--filter", f"label={OWNER}={instance}",
        "--filter", f"name=^/{ENVIRONMENT}-{instance}-graph$",
    ).decode().splitlines()
    if ids:
        docker(directory, "rm", "-fv", *ids)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("graph_repo", type=Path)
    parser.add_argument("older_revision")
    parser.add_argument("newer_revision")
    args = parser.parse_args()
    if git(args.graph_repo, "remote", "get-url", "origin").removesuffix(".git") != SOURCE:
        parser.error("expected the aq-graph origin")
    revisions = (args.older_revision, args.newer_revision)
    history = git(args.graph_repo, "rev-list", "--first-parent", "refs/remotes/origin/main").splitlines()
    if (any(revision not in history for revision in revisions)
            or history.index(revisions[0]) <= history.index(revisions[1])):
        parser.error("supply two full aq-graph revisions in first-parent origin/main order")

    state = ROOT / "state" / ENVIRONMENT
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    with TemporaryDirectory(dir=state) as work, ExitStack() as cleanup:
        directory = Path(work)
        instance = uuid4().hex[:12]
        print(f"Environment {ENVIRONMENT}-{instance}", flush=True)
        (directory / "provenance").mkdir()
        (directory / "docker").mkdir()
        (directory / "docker/config.json").write_text("{}\n")
        (directory / "environment.json").write_text(json.dumps({
            "environment": ENVIRONMENT, "instance": instance,
        }))
        for revision in revisions:
            build(directory, args.graph_repo, revision, instance, cleanup)
        database_url = provision(directory, instance, cleanup)
        env = {
            **os.environ, "DATABASE_URL": database_url,
            "AQ_GRAPH_DEPLOY_CANARY": str(directory), "AQ_GRAPH_DEPLOY_REVISIONS": ",".join(revisions),
            "CARGO_PROFILE_TEST_DEBUG": os.environ.get("CARGO_PROFILE_TEST_DEBUG", "0"),
            "CARGO_TARGET_DIR": str(ROOT / "target"),
        }
        subprocess.run(
            ["timeout", "900", "cargo", "test", "--locked", "-p", "aq-server", "--test", "it_playbook",
             "--", TEST, "--exact", "--ignored", "--nocapture"],
            env=env, cwd=ROOT, check=True,
        )


if __name__ == "__main__":
    main()
