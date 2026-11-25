#!/usr/bin/env python3
"""Stage two local repository identities behind a private real aq-git server."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import threading
import time
from uuid import UUID, uuid4

GIT_IMAGE = "sha256:d93060ac2e0d87f5ec5785e58dc6371c9e210d44dfe386188e5b2991f615424d"


def run(*command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


def git_binary(directory, image):
    binary = directory / "aq-git"
    container = run("docker", "create", "--pull", "never", image,
                    text=True, capture_output=True).stdout.strip()
    try:
        run("docker", "cp", container + ":/usr/local/bin/aq-git", binary)
    finally:
        run("docker", "rm", container, stdout=subprocess.DEVNULL)
    return binary


def fixture_repo(directory, name):
    repo = directory / name
    git = ("git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false")
    run(*git, "init", "--quiet", "--initial-branch=main", repo)
    def commit(marker):
        (repo / "README.md").write_text(marker + "\n")
        run(*git, "-C", repo, "add", "README.md")
        run(*git, "-C", repo, "-c", "user.name=AQ Pilot",
            "-c", "user.email=aq-pilot@example.test", "commit", "-qm", "Seed pilot base")
        return run(*git, "-C", repo, "rev-parse", "HEAD",
                   text=True, capture_output=True).stdout.strip()

    main = commit(f"Disposable {name} main fixture")
    if name == "aq-server":
        return repo, main, main, ["refs/heads/main"]
    run(*git, "-C", repo, "switch", "--quiet", "--orphan", "workspace-source")
    pin = commit("Disposable dashboard pin with unrelated history")
    run(*git, "-C", repo, "switch", "--quiet", "main")
    unrelated = subprocess.run((*git, "-C", repo, "merge-base", main, pin),
                               capture_output=True, check=False)
    if unrelated.returncode != 1:
        raise RuntimeError("sibling fixture pin must be independent of its default branch")
    return repo, pin, main, ["refs/heads/main", "refs/heads/workspace-source"]


def request(binary, address, operation, repository, params, generation=None):
    envelope = {
        "protocol_version": 1,
        "request_id": str(uuid4()),
        "operation": operation,
        "repository_id": repository,
        "params": params,
    }
    if generation is not None:
        envelope.update(operation_id=str(uuid4()), expected_binding_generation=generation)
    process = subprocess.run((binary, "--socket", address, "--json", "request"),
                             input=json.dumps(envelope), text=True, capture_output=True,
                             check=False)
    result = json.loads(process.stdout)
    if process.returncode != 0 or not result["ok"]:
        raise RuntimeError(f"aq-git {operation} refused the private fixture: {result}")
    return result["result"]


def verify_export(directory, name, artifact, manifest):
    bundle = directory / "state" / "artifacts" / str(UUID(artifact)) / "objects.bundle"
    if hashlib.sha256(bundle.read_bytes()).hexdigest() != manifest["bundle_sha256"]:
        raise RuntimeError("pilot bundle bytes disagree with the retained manifest")
    receiver = directory / f"{name}-bundle-proof"
    run("git", "init", "--bare", "--quiet", receiver)
    run("git", "-C", receiver, "bundle", "unbundle", bundle, stdout=subprocess.DEVNULL)
    run("git", "-C", receiver, "cat-file", "-e", manifest["base"]["hex"] + "^{commit}")
    content = run("git", "-C", receiver, "show",
                  manifest["source"]["hex"] + ":pilot-change.txt", capture_output=True)
    if content.stdout != f"Retained change in {name}\n".encode():
        raise RuntimeError("fresh Git receiver did not recover the repository-specific change")


def retain(binary, address, directory, name, repository, base):
    base_object = {"algorithm": "sha1", "hex": base}
    workspace = request(binary, address, "workspace.create", repository,
                        {"base": base_object}, generation=1)
    checkout = Path(workspace["path"]).resolve(strict=True)
    if not checkout.is_relative_to(directory / "state"):
        raise RuntimeError("aq-git checkout escaped the private pilot root")
    (checkout / "pilot-change.txt").write_text(f"Retained change in {name}\n")
    checkpoint = request(binary, address, "workspace.checkpoint", repository, {
        "workspace_id": workspace["workspace_id"],
        "expected_generation": workspace["generation"],
        "expected_head": base_object,
        "quiescent": True,
        "mode": "all",
        "commit": {"name": "AQ Pilot", "email": "aq-pilot@example.test",
                   "message": f"Checkpoint pilot change in {name}"},
    }, generation=1)
    if checkpoint["unchanged"] or checkpoint["source"] == base_object:
        raise RuntimeError("pilot checkout did not retain a changed commit")
    exported = request(binary, address, "artifact.export", repository, {
        "source": checkpoint["source"],
        "base": base_object,
        "retention_handle": checkpoint["retention_handle"],
    }, generation=1)
    manifest = exported["manifest"]
    if manifest["repository_id"] != repository or manifest["source"] != checkpoint["source"]:
        raise RuntimeError("exported pilot artifact changed repository or commit")
    verify_export(directory, name, exported["artifact_handle"], manifest)
    base_handle = workspace["workspace_id"]
    binding = request(binary, address, "retention.inspect", repository,
                      {"handle_id": base_handle})
    if (binding["id"] != base_handle or binding["state"] != "active"
            or not any(ref["oid"] == base for ref in binding["refs"])):
        raise RuntimeError("pilot base is not independently retained")
    return {"base_retention_handle": base_handle,
            "artifact_handle": exported["artifact_handle"],
            "retention_handle": checkpoint["retention_handle"],
            "head_oid": checkpoint["source"]["hex"],
            "bundle_sha256": manifest["bundle_sha256"]}


def seed(binary, address, directory, name):
    repo, oid, main, refs = fixture_repo(directory, name)
    repo_id = str(uuid4())
    request(binary, address, "repo.register", repo_id, {
        "binding": {
            "identity": f"task-workspace-pilot/{name}",
            "mode": "central",
            "binding_generation": 1,
            "remote_url": repo.as_uri(),
            "allow_file_transport": True,
            "allowed_refs": refs,
        },
    }, generation=0)
    request(binary, address, "repo.sync", repo_id, {}, generation=1)
    inspected = request(binary, address, "inspect", repo_id,
                        {"oid": {"algorithm": "sha1", "hex": oid}})
    if inspected["oid"] != {"algorithm": "sha1", "hex": oid}:
        raise RuntimeError("aq-git returned an unrelated commit")
    return {"repository_id": repo_id, "binding_generation": 1, "commit_oid": oid,
            "default_head": main,
            "retained_fixture": retain(binary, address, directory, name, repo_id, oid)}


def wait_for_socket(service, address):
    for _ in range(120):
        if service.poll() is not None:
            raise RuntimeError(f"private aq-git server exited {service.returncode}")
        try:
            with socket.socket(socket.AF_UNIX) as client:
                client.connect(str(address))
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("private aq-git socket did not become ready")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, help="existing empty private directory")
    parser.add_argument("--git-image", default=GIT_IMAGE)
    args = parser.parse_args()
    os.umask(0o077)
    directory = args.directory.resolve(strict=True)
    if directory.stat().st_mode & 0o777 != 0o700 or any(directory.iterdir()):
        parser.error("directory must be empty and mode 0700")
    binary = git_binary(directory, args.git_image)
    address = directory / "git.sock"
    git_state = directory / "state"
    git_state.mkdir()
    stopping = threading.Event()
    for name in (signal.SIGINT, signal.SIGTERM):
        signal.signal(name, lambda *_: stopping.set())
    with (directory / "aq-git.log").open("w") as log:
        service = subprocess.Popen((binary, "--state-dir", git_state, "--listen",
                                    address, "serve"), stdout=log, stderr=log)
        try:
            wait_for_socket(service, address)
            evidence = {name: seed(binary, address, directory, name)
                        for name in ("aq-server", "aq-dashboard")}
            evidence["socket"] = str(address)
            temporary = directory / "repositories.tmp"
            temporary.write_text(json.dumps(evidence, indent=2) + "\n")
            temporary.rename(directory / "repositories.json")
            print(json.dumps(evidence), flush=True)
            while not stopping.wait(0.2) and service.poll() is None:
                pass
        finally:
            if service.poll() is None:
                service.terminate()
                service.wait()


if __name__ == "__main__":
    main()
