#!/usr/bin/env python3
"""Build two merged aq-graph revisions and qualify the Host11 deployment playbook."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import re
import runpy
import secrets
import shutil
import subprocess
from tempfile import TemporaryDirectory
import time
from uuid import uuid4

ROOT = Path(__file__).resolve().parents[1]
ENVIRONMENT = "aq-graph-deploy-canary"
OWNER = "io.aq.graph-deploy.environment"
TEST_PREFIX = "real_adapter_pilots::deployment_canary"
BUILDER = runpy.run_path(str(ROOT / "deploy/graph-deployment/build-image.py"))
build, docker, history, private = (BUILDER[key] for key in ("build", "docker", "history", "private"))


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
    parser.add_argument("--automatic", action="store_true")
    parser.add_argument("--merge-client", type=Path, help="absolute path to psql (defaults to PATH)")
    parser.add_argument("graph_repo", type=Path)
    parser.add_argument("older_revision")
    parser.add_argument("newer_revision")
    args = parser.parse_args()
    client = args.merge_client or shutil.which("psql")
    if args.automatic and not client:
        parser.error("automatic canary requires psql")
    revisions = (args.older_revision, args.newer_revision)
    merged = history(args.graph_repo)
    if (any(revision not in merged for revision in revisions)
            or merged.index(revisions[0]) <= merged.index(revisions[1])):
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
        for revision in revisions[1:] if args.automatic else revisions:
            build(directory, args.graph_repo, revision, instance, cleanup)
        database_url = provision(directory, instance, cleanup)
        env = {
            **os.environ, "DATABASE_URL": database_url,
            "AQ_GRAPH_DEPLOY_CANARY": str(directory), "AQ_GRAPH_DEPLOY_REVISIONS": ",".join(revisions),
            "CARGO_PROFILE_TEST_DEBUG": os.environ.get("CARGO_PROFILE_TEST_DEBUG", "0"),
            "CARGO_TARGET_DIR": str(ROOT / "target"),
        }
        test = "merged_graph_rolls_out_by_verified_digest"
        if args.automatic:
            test = "automatic::host10_evidence_drives_host11"
            env["AQ_GRAPH_DEPLOY_PSQL"] = str(Path(client).absolute())
            env["AQ_GRAPH_DEPLOY_GRAPH_REPO"] = str(args.graph_repo.resolve())
        subprocess.run(
            ["timeout", "900", "cargo", "test", "--locked", "-p", "aq-server",
             "--test", "it_playbook", "--", f"{TEST_PREFIX}::{test}", "--exact", "--ignored", "--nocapture"],
            env=env, cwd=ROOT, check=True,
        )


if __name__ == "__main__":
    main()
