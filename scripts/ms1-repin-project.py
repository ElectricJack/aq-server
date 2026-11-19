#!/usr/bin/env python3
"""Re-pin an aq-ms1 project's starting commit to its repository's current main.

A project whose source is `immutable` in `[profiled_execution] projects` starts every task
from one fixed commit. Nothing moves that pin, so work merged on GitHub after the project was
set up is invisible to its tasks. Until the server can follow a branch that others push to,
run this after merges and before queuing work that must see them:

  scripts/ms1-repin-project.py PROJECT_ID            # show what would change
  scripts/ms1-repin-project.py PROJECT_ID --apply    # change the config and restart the server

It syncs main in the stack's central aq-git, retains that commit (a retention handle holds
exactly one commit), rewrites the project's entry in the vault config, raises the
`[profiled_execution]` revision (the server refuses changed content under an old revision)
and recreates the server container. The server is unavailable for about a minute.

Only a project already listed with an `immutable` source is changed. The previous config is
kept in the vault as config.toml.bak-repin-<time>.

--apply does not trust the --compose command. It records the server container's id and start time
before the command runs and exits non-zero unless, within --wait-seconds, a different container
(or a different start time) is running and the server's /healthz answers. "The server was not
recreated; the new pin is written but not loaded" means the command ran without recreating it:
the vault config holds the new pin, the old server still runs with the old one. The server has no
read of a project's effective source commit, so the script cannot confirm the commit itself; it
proves only that the server restarted and is healthy after the config was written.
"""
import argparse
import json
import re
import subprocess
import sys
import time
import tomllib
import uuid

STACK = "aq-ms1"
CONFIG = "/vault/settlement/config.toml"
BRANCH = "refs/heads/main"


def docker(args, stdin=None, tries=3):
    """Docker Desktop sometimes returns empty output for a read; reads are retried."""
    for _ in range(tries):
        done = subprocess.run(["docker", *args], input=stdin, capture_output=True, text=True, timeout=600)
        if done.stdout.strip() or stdin is not None:
            return done
        time.sleep(2)
    return done


def server_identity():
    """(container id, start time) of the running server container, or None while it is absent or stopped."""
    done = subprocess.run(
        ["docker", "inspect", f"{STACK}-server-1", "--format", "{{.Id}} {{.State.StartedAt}} {{.State.Running}}"],
        capture_output=True, text=True, timeout=60)
    fields = done.stdout.split()
    if done.returncode != 0 or len(fields) != 3 or fields[2] != "true":
        return None
    return fields[0], fields[1]


def server_healthy():
    """The server's own /healthz probe (the image has no curl), run inside the container."""
    done = subprocess.run(["docker", "exec", f"{STACK}-server-1", "aq-server", "healthcheck"],
                          capture_output=True, text=True, timeout=60)
    return done.returncode == 0


def wait_for_recreated(before, wait_seconds, poll_seconds):
    """True once a container other than `before` (different id or start time) is running and healthy."""
    deadline = time.monotonic() + wait_seconds
    while True:
        now = server_identity()
        if now is not None and now != before and server_healthy():
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(poll_seconds)


def git_request(repository, operation, params=None, mutation=None):
    request = dict(protocol_version=1, request_id=str(uuid.uuid4()), operation=operation, params=params or {},
                   deadline_ms=120000, repository_id=repository, expected_binding_generation=1)
    if mutation:
        request["operation_id"] = mutation
    args = ["exec", "-i", f"{STACK}-aq-git-1", "aq-git", "--socket", "/run/aq-git/central.sock",
            "--input", "-", "--json", "request"]
    raw = ""
    for _ in range(3):  # the same operation id makes a repeated mutation a replay
        raw = docker(args, stdin=json.dumps(request) + "\n").stdout
        if raw.strip():
            break
        time.sleep(2)
    response = json.loads(raw)
    if response.get("ok") is not True:
        raise SystemExit(f"aq-git refused {operation}: {json.dumps(response.get('error'))[:500]}")
    return response["result"]


def retain_main(repository, tag):
    def op(step):
        return str(uuid.uuid5(uuid.NAMESPACE_URL, f"ms1-repin-{repository}-{tag}-{step}"))
    synced = git_request(repository, "repo.sync", dict(refs=[BRANCH]), mutation=op("sync"))
    head = synced.get("refs", {}).get(BRANCH)
    if not (isinstance(head, str) and len(head) == 40):
        raise SystemExit("sync gave no main commit")
    base = dict(algorithm="sha1", hex=head)
    created = git_request(repository, "workspace.create", dict(base=base), mutation=op("workspace"))
    checkpoint = git_request(repository, "workspace.checkpoint", dict(
        workspace_id=created["workspace_id"], expected_generation=1, expected_head=base, quiescent=True,
        mode="head"), mutation=op("checkpoint"))
    git_request(repository, "workspace.retire", dict(
        workspace_id=created["workspace_id"], expected_generation=checkpoint["workspace_generation"],
        expected_head=base, quiescent=True), mutation=op("retire"))
    retained = git_request(repository, "retention.inspect", dict(handle_id=checkpoint["retention_handle"]))
    if retained["state"] != "active" or not any(r["oid"] == head for r in retained["refs"]):
        raise SystemExit("the retention handle does not hold the synced commit")
    return head, checkpoint["retention_handle"]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("project")
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--compose", default="",
                        help="shell command that recreates the server container, given as ONE quoted argument "
                        "including its arguments, e.g. --compose 'compose.sh after up -d --no-deps --no-build "
                        "--force-recreate --wait server'. A bare 'compose.sh' (no arguments) or a command "
                        "without --force-recreate does nothing: after it runs the script requires a new "
                        "server container that is healthy, and exits non-zero otherwise")
    parser.add_argument("--wait-seconds", type=float, default=180.0,
                        help="how long after --compose to wait for a new, healthy server container (default 180)")
    parser.add_argument("--poll-seconds", type=float, default=2.0,
                        help="seconds between checks while waiting (default 2)")
    args = parser.parse_args()

    text = docker(["exec", f"{STACK}-server-1", "cat", CONFIG]).stdout
    if not text.strip():
        raise SystemExit("could not read the server's config")
    config = tomllib.loads(text)
    section = config["profiled_execution"]
    entry = next((p for p in section["projects"] if p["project_id"] == args.project), None)
    if entry is None or entry["source"].get("kind") != "immutable":
        raise SystemExit("the project is not listed with an immutable source; nothing to re-pin")
    old_hex, old_handle = entry["source"]["object"]["hex"], entry["retention_handle"]

    repository = docker(["exec", f"{STACK}-postgres-1", "sh", "-ec",
                         'PGPASSWORD="$(cat /run/secrets/postgres_password)" psql -h 127.0.0.1 -U agent_q -d agent_q -tAc "$0"',
                         "SELECT binding->>'repository_id' FROM project_execution_bindings WHERE project_id='%s'"
                         % str(uuid.UUID(args.project))]).stdout.strip()
    if not repository:
        raise SystemExit("the project has no repository binding")

    tag = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    head, handle = retain_main(repository, tag)
    print(f"project {args.project}: pinned {old_hex[:12]}, main is {head[:12]}")
    if head == old_hex:
        print("already at main; nothing to do")
        return 0
    if text.count(old_hex) != 1 or text.count(old_handle) != 1:
        raise SystemExit("the pinned commit or its handle is not unique in the config; edit it by hand")
    start = text.index("[profiled_execution]")
    end = text.find("\n[", start + 1)
    end = len(text) if end < 0 else end
    block, revision = text[start:end], section["revision"]
    bumped, count = re.subn(rf"(?m)^revision = {revision}$", f"revision = {revision + 1}", block)
    if count != 1:
        raise SystemExit("could not find the section's revision line")
    updated = (text[:start] + bumped + text[end:]).replace(old_hex, head).replace(old_handle, handle)
    check = tomllib.loads(updated)["profiled_execution"]
    assert check["revision"] == revision + 1 and any(
        p["source"].get("object", {}).get("hex") == head and p["retention_handle"] == handle for p in check["projects"])
    if not args.apply:
        print(f"would set revision {revision + 1} and retention handle {handle}; pass --apply to do it")
        return 0
    if not args.compose:
        raise SystemExit("--apply needs --compose, the command that recreates the server container")
    before = server_identity()
    if before is None:
        raise SystemExit("the server container is not running; nothing was changed")
    image = docker(["inspect", f"{STACK}-server-1", "--format", "{{.Config.Image}}"]).stdout.strip()
    write = subprocess.run(
        ["docker", "run", "--rm", "-i", "--user", "10001", "--entrypoint", "sh", "-v", f"{STACK}_dev_vault:/vault", image,
         "-c", f"umask 077; cp -p {CONFIG} {CONFIG}.bak-repin-{tag} && cat > {CONFIG}.next && mv {CONFIG}.next {CONFIG}"],
        input=updated, capture_output=True, text=True, timeout=120)
    if write.returncode != 0:
        raise SystemExit(f"config not written: {write.stderr[:300]}")
    print(f"config written (previous kept as config.toml.bak-repin-{tag}); recreating the server")
    code = subprocess.run(args.compose, shell=True).returncode
    if code != 0:
        print(f"--compose command failed with exit code {code}; the new pin is written but may not be loaded",
              file=sys.stderr)
        return code
    if not wait_for_recreated(before, args.wait_seconds, args.poll_seconds):
        raise SystemExit(f"the server was not recreated; the new pin is written but not loaded "
                         f"(no new healthy {STACK}-server-1 container within {args.wait_seconds:g}s after "
                         f"--compose; the old server still runs with the old pin)")
    print("the server was recreated and is healthy (its effective source commit is not readable, so the pin "
          "itself is not confirmed)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
