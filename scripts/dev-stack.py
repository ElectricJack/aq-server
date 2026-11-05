#!/usr/bin/env python3
"""Private, replayable development configuration and public-API seed."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import secrets
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from migration_catalog import state_format

ROOT = Path(__file__).resolve().parents[1]
WORKSPACE = "e0000000-0000-4000-8000-000000000001"
PROJECT = "e0000000-0000-4000-8000-000000000002"
REPOSITORY = "e0000000-0000-4000-8000-000000000003"
TASK = "e0000000-0000-4000-8000-000000000004"


def private_file(path, content):
    """Create once, refusing unsafe retained files without printing their bytes."""
    try:
        metadata = path.lstat()
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
                or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077
                or metadata.st_size == 0):
            raise ValueError("unsafe retained development settlement file") from None
    except FileNotFoundError:
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".dev-settlement-", delete=False) as out:
                temporary = Path(out.name)
                out.write(content)
                out.flush()
                os.fsync(out.fileno())
            os.link(temporary, path)
        finally:
            if temporary is not None:
                temporary.unlink()


def paths():
    state = Path(os.environ.get("AQ_DEV_STATE_DIR", ROOT / "state.dev")).resolve()
    secret = Path(os.environ.get("AQ_SECRETS_DIR", ROOT / "secrets.dev")).resolve()
    vault = Path(os.environ.get("AQ_VAULT_DIR", state / "vault")).resolve()
    return state, secret, vault


def prepare():
    state, secret, vault = paths()
    for directory in (state, secret, vault,
                      state / "updater-spool/requests", state / "updater-spool/status"):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        directory.chmod(0o700)
    # Only disposable, non-secret test repositories are mounted here. The
    # private state parent still excludes other host users.
    (state / "fixture").mkdir(exist_ok=True, mode=0o755)
    (state / "fixture").chmod(0o755)
    for name, mode in (("postgres_password", 0o644), ("graph_database_password", 0o644), ("graph_token", 0o600)):
        path = secret / name
        try:
            with path.open("x") as out:
                out.write(secrets.token_hex(32))
        except FileExistsError:
            if path.is_symlink() or not path.is_file() or path.stat().st_size < 32:
                raise ValueError("invalid existing development credential file")
        path.chmod(mode)
    private_file(secret / "playbook_database_password", secrets.token_hex(32).encode())
    private_file(secret / "playbook_engine_keys", json.dumps(dict(active_key_id="dev-engine-1",
        keys={"dev-engine-1": secrets.token_hex(32)})).encode())
    private_file(secret / "playbook_bridge_key", secrets.token_bytes(32))
    private_file(secret / "terminal_protection_key", secrets.token_bytes(32))
    config = (ROOT / "vault.example/config.toml").read_text()
    config += '''
[agent_access]
graph_url = "http://aq-graph:8741"
graph_token_file = "/run/aq-dev/graph_token"
mutation_actor_id = "dev-service"
[source_retention]
staging_root = "/var/lib/aq/source-staging"
git_root = "/var/lib/aq/git"
git_socket = "/run/aq-git/central.sock"
[terminals.protection]
active_key_id = "dev-terminal-1"
[[terminals.protection.keys]]
id = "dev-terminal-1"
file = "/run/aq-dev/terminal_protection_key"
'''
    target = vault / "config.toml"
    if target.exists() and target.read_text() != config:
        raise ValueError("development vault differs from generated configuration; preserve it and use a new AQ_DEV_STATE_DIR")
    target.write_text(config)
    return dict(AQ_STATE_FORMAT=str(state_format(ROOT)), AQ_DEV_STATE_DIR=str(state),
                AQ_VAULT_DIR=str(vault), AQ_SECRETS_DIR=str(secret),
                AQ_UPDATER_SPOOL=str(state / "updater-spool"), AQ_DEV_UID=str(os.getuid()),
                AQ_DEV_GID=str(os.getgid()), AQ_DEV_GRAPH_WORKSPACE=WORKSPACE,
                AQ_SERVER_IMAGE=os.environ.get("AQ_SERVER_IMAGE", "agent-q-server:dev"))


def compose(env, *args, capture=True, input=None, timeout=None):
    result = subprocess.run(["docker", "compose", "-f", str(ROOT / "compose.yaml"),
                             "-f", str(ROOT / "compose.dev.yaml"), *args],
                            cwd=ROOT, env=dict(os.environ, **env), text=True,
                            capture_output=capture, input=input, timeout=timeout)
    if result.returncode:
        # Output is retained by callers; commands never contain credential values.
        raise RuntimeError(f"development Compose command exited {result.returncode}: {result.stderr}" if capture else "development Compose command failed")
    return result.stdout if capture else None


def enable_settlement():
    """Activate only after public-API seed has created the real owner/project."""
    env = prepare()
    state, _, _ = paths()
    with (state / ".settlement.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return enable_settlement_locked(env)


def enable_settlement_locked(env):
    state, _, vault = paths()
    receipt = state / "seed.json"
    if not receipt.exists() or json.loads(receipt.read_text()).get("project_id") != PROJECT:
        raise ValueError("settlement requires a completed development seed")
    server = os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:8080")
    users = http(server + "/api/users")
    owners = [user for user in users if user["email"] == "owner@localhost" and user["role"] == "owner"]
    if len(owners) != 1:
        raise ValueError("settlement requires the current development owner")
    owner_id = http(server + "/api/v1/bootstrap")["data"]["principal"]["id"]
    project = http(server + "/api/projects/" + PROJECT)
    if not project or project["binding"]["graph_workspace_id"] != WORKSPACE or not project["binding"]["enabled"]:
        raise ValueError("settlement project differs from the development seed")
    # Discover the actual socket UID; do not assume the image's postgres UID.
    socket_uid = int(compose(env, "exec", "-T", "postgres", "stat", "-c", "%u",
        "/run/aq-postgresql/.s.PGSQL.5432").strip())
    pins_path = state / "settlement.json"
    private_file(pins_path, json.dumps(dict(namespace_id=str(uuid.uuid4()), grant_id=str(uuid.uuid4()),
        owner_user_id=owner_id, socket_owner_uid=socket_uid,
        expires_at_ms=int(time.time() * 1000) + 23 * 60 * 60 * 1000), sort_keys=True).encode())
    pins = json.loads(pins_path.read_text())
    if pins["owner_user_id"] != owner_id or pins["socket_owner_uid"] != socket_uid:
        raise ValueError("retained settlement owner/socket differs; use a fresh development stack")
    artifact = (ROOT / "scripts/dev-playbook-artifact.json").read_bytes()
    private_file(vault / "playbook-artifact.json", artifact)
    if (vault / "playbook-artifact.json").read_bytes() != artifact:
        raise ValueError("retained settlement artifact differs; use a fresh development stack")
    config = (vault / "config.toml").read_text() + f'''
[playbooks]
enabled = true
profile = "human_review_v2"
engine_connection_file = "/run/aq-dev/playbook_dsn"
socket_owner_uid = {socket_uid}
engine_keyring_file = "/run/aq-dev/playbook_engine_keys"
drive_interval_ms = 1000
[playbooks.namespace]
namespace_id = "{pins['namespace_id']}"
generation = 1
name = "E0 local human settlement"
workspace_id = "{WORKSPACE}"
project_id = "{PROJECT}"
owner_user_id = "{pins['owner_user_id']}"
[playbooks.protection]
active_key_id = "dev-bridge-1"
[[playbooks.protection.keys]]
id = "dev-bridge-1"
file = "/run/aq-dev/playbook_bridge_key"
[[playbooks.deployments]]
artifact_file = "/run/aq-dev/playbook-artifact.json"
binding = "development-inspection-only"
grant_id = "{pins['grant_id']}"
expires_at_ms = {pins['expires_at_ms']}
[playbooks.deployments.scope]
workspace_id = "{WORKSPACE}"
project_id = "{PROJECT}"
capabilities = ["operation.get"]
[[playbooks.deployments.scope.recipients]]
kind = "task"
workspace_id = "{WORKSPACE}"
project_id = "{PROJECT}"
task_id = "{TASK}"
'''
    private_file(vault / "playbooks.toml", config.encode())
    if (vault / "playbooks.toml").read_text() != config:
        raise ValueError("retained settlement configuration differs; use a fresh development stack")
    # Stop gracefully before acquiring the retained engine owner on a new process.
    compose(env, "stop", "server")
    compose(env, "run", "--rm", "--no-deps", "dev-state-init")
    compose(env, "up", "-d", "--no-build", "--wait", "--wait-timeout", "120", "server")
    status = http(server + "/api/playbooks/status")
    if status["namespace"]["namespace_id"] != pins["namespace_id"]:
        raise ValueError("settlement owner did not activate the retained namespace")
    return dict(namespace_id=pins["namespace_id"], grant_id=pins["grant_id"],
                expires_at_ms=pins["expires_at_ms"])


def designation(local_fixture=False):
    keys = ("DEV_SCRATCH_REPO_URL", "DEV_SCRATCH_BASE_BRANCH", "DEV_SCRATCH_CREDENTIAL_REF")
    if any(not os.environ.get(key) for key in keys):
        raise ValueError("seed requires explicit DEV_SCRATCH_REPO_URL, DEV_SCRATCH_BASE_BRANCH and DEV_SCRATCH_CREDENTIAL_REF (use 'none' for a public repository); no scratch repository is designated by default")
    url, branch, credential = (os.environ[key] for key in keys)
    parsed = urllib.parse.urlsplit(url)
    if parsed.username or parsed.password or parsed.query or parsed.fragment or any(c.isspace() for c in url):
        raise ValueError("DEV_SCRATCH_REPO_URL must not contain credentials, query, fragment or whitespace")
    if not (parsed.scheme == "https" and parsed.hostname == "github.com" and len(parsed.path.strip("/").split("/")) == 2):
        if not (local_fixture and url.startswith("/fixture/") and not parsed.scheme):
            raise ValueError("DEV_SCRATCH_REPO_URL must designate an HTTPS GitHub repository")
    if subprocess.run(["git", "check-ref-format", "refs/heads/" + branch], capture_output=True).returncode:
        raise ValueError("invalid DEV_SCRATCH_BASE_BRANCH")
    if credential != "none":
        raise ValueError("pinned aq-git does not enable named credential profiles; DEV_SCRATCH_CREDENTIAL_REF must explicitly be 'none' for a public scratch repository")
    return dict(url=url, base_branch=branch, credential_ref=credential, local_fixture=local_fixture)


def http(url, body=None, token=None):
    headers = {"Content-Type": "application/json", "X-AQ-Request": "1"}
    if token:
        headers["Authorization"] = "Bearer " + token
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        # Do not print request credentials or server response bodies.
        raise RuntimeError(f"public API returned HTTP {error.code}") from None


def git_request(state, operation, params=None, mutation=None):
    request = dict(protocol_version=1, request_id=str(uuid.uuid4()), operation=operation,
                   params=params or {}, deadline_ms=10000)
    if operation != "capabilities":
        request.update(repository_id=REPOSITORY, expected_binding_generation=0 if operation == "repo.register" else 1)
    if mutation:
        request["operation_id"] = mutation
    if state.resolve() != paths()[0]:
        raise ValueError("Git request state must match the selected development stack")
    try:
        raw = compose(prepare(), "exec", "-T", "aq-git", "aq-git", "--socket", "/run/aq-git/central.sock",
                      "--input", "-", "--json", "request", input=json.dumps(request) + "\n", timeout=15)
    except subprocess.TimeoutExpired:
        raise RuntimeError("aq-git request timed out; replay the same seed operation") from None
    if len(raw.encode()) > 65536 or not raw.endswith("\n"):
        raise RuntimeError("invalid bounded aq-git response")
    response = json.loads(raw)
    if response.get("request_id") != request["request_id"] or response.get("protocol_version") != 1 or response.get("ok") is not True:
        raise RuntimeError("aq-git refused " + operation)
    return response["result"]


def seed(local_fixture=False):
    choice = designation(local_fixture)  # Validate before creating files or contacting services.
    prepare()
    state, _, _ = paths()
    with (state / "seed.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return seed_locked(choice, local_fixture)


def verify_worker_credential(record, node_id, worker_url):
    if (not isinstance(record, dict) or record.get("node_id") != node_id
            or not isinstance(record.get("credential"), str)
            or not record["credential"].startswith("aqn_")
            or not isinstance(record.get("heartbeat_interval_secs"), int)
            or record["heartbeat_interval_secs"] <= 0):
        raise ValueError("saved worker credential is incomplete or belongs to another node; existing identities are preserved")
    nonce = uuid.uuid4().hex
    try:
        reply = http(worker_url + "/v1/worker/probe", dict(nonce=nonce), record["credential"])
    except (RuntimeError, OSError):
        raise ValueError("saved worker credential could not be authenticated; existing identities are preserved") from None
    if not isinstance(reply, dict) or reply.get("nonce") != nonce:
        raise ValueError("worker credential probe response was not correlated; existing identities are preserved")


def save_worker_credential(path, record):
    # Publish a complete private file atomically without replacing retained credentials.
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=".dev-worker-", delete=False) as output:
            temporary = Path(output.name)
            json.dump(record, output)
            output.flush()
            os.fsync(output.fileno())
        os.link(temporary, path)
        directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if temporary is not None:
            temporary.unlink()


def seed_locked(choice, local_fixture):
    state, secret, _ = paths()
    receipt = state / "seed.json"
    if receipt.exists() and json.loads(receipt.read_text())["designation"] != choice:
        raise ValueError("seed designation changed; existing project is preserved; use a new AQ_DEV_STATE_DIR and Compose project")
    # Pin replay inputs before the first mutation, so partial runs cannot switch repositories.
    receipt.write_text(json.dumps(dict(designation=choice), sort_keys=True) + "\n")
    graph = os.environ.get("AQ_DEV_GRAPH_URL", "http://127.0.0.1:8741")
    server = os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:8080")
    worker_url = os.environ.get("AQ_DEV_WORKER_URL", "http://127.0.0.1:8081")
    workers = [worker for worker in http(server + "/api/workers") if worker["name"] == "e0-local-development"]
    if len(workers) > 1 or any(worker["status"] == "revoked" for worker in workers):
        raise ValueError("seed worker identity is ambiguous or revoked; existing identities are preserved")
    credential_file = secret / "dev_worker.json"
    if workers:
        node_id = workers[0]["id"]
        try:
            if credential_file.is_symlink() or not credential_file.is_file():
                raise ValueError()
            worker = json.loads(credential_file.read_text())
        except (ValueError, OSError):
            raise ValueError("saved worker credential is missing or invalid; existing identities are preserved; recover the credential or use a new state, secrets and Compose project") from None
        verify_worker_credential(worker, node_id, worker_url)
    else:
        if credential_file.exists() or credential_file.is_symlink():
            raise ValueError("saved worker credential has no matching worker; existing identities are preserved; use a new state, secrets and Compose project")
        enrollment = http(server + "/api/enrollments", {})
        worker = http(worker_url + "/v1/worker/enroll", dict(code=enrollment["code"],
            name="e0-local-development", worker_version="e0-seed", protocol_version=1))
        node_id = worker["node_id"]
        verify_worker_credential(worker, node_id, worker_url)
        save_worker_credential(credential_file, worker)
    token = (secret / "graph_token").read_text().strip()
    workspace = graph + "/v1/workspaces/" + WORKSPACE
    if http(workspace + "/nodes/" + TASK, token=token) is None:
        http(workspace + "/mutations", dict(idempotency_key="e0-development-task-v1", reason="E0 local development seed",
             operations=[dict(op="create_node", id=TASK, kind="task", title="Local development scratch task",
                              body="Use only the owner-designated scratch repository.", task_state="active")]), token)
    # Stable operation ID makes response-loss replay safe at the Git authority.
    git_request(state, "repo.register", dict(binding=dict(identity="e0-development", mode="central",
        remote_url=choice["url"], allow_file_transport=local_fixture,
        allowed_refs=["refs/heads/" + choice["base_branch"]],
        allowed_targets=["refs/heads/" + choice["base_branch"]])),
        mutation="e0000000-0000-4000-8000-000000000006")
    # This immutable artifact is generated from the checked-in example catalog.
    artifact = (ROOT / "scripts/dev-catalog.json").read_text().strip()
    digest = hashlib.sha256(artifact.encode()).hexdigest()
    active = http(server + "/api/profiles/catalogs")
    if active["active_digest"] is None:
        http(server + "/api/profiles/catalogs/activate", dict(operation_id="e0000000-0000-4000-8000-000000000007",
             expected_revision=0, artifact_json=artifact))
    elif active["active_digest"] != digest:
        raise ValueError("active catalog differs; seed preserves the existing catalog")
    current = http(server + "/api/projects/" + PROJECT)
    if current is None:
        binding = dict(schema_version=1, project_id=PROJECT, name="Local scratch development", enabled=True,
            repository_id=REPOSITORY, repository_binding_generation=1, repository_adapter_reference="aq.git.configured",
            graph_adapter_reference="aq.graph.configured", graph_workspace_id=WORKSPACE, catalog_digest=digest,
            allowed_agent_profiles=["implementer@1"], allowed_intelligence_profiles=["engineering@1"],
            allowed_execution_profiles=["confined-development@1"], allowed_node_ids=[node_id], allowed_worker_groups=[],
            allowed_environments=["confined-worktree"], policy=dict(identity="local-development", revision="1", digest="b" * 64),
            context=[], ceilings=dict(permissions=[dict(operation="task.read", scope_reference="project:example")],
                context_bytes=8192, wall_time_ms=60000, memory_bytes=268435456, cpu_millis=60000, process_limit=8,
                cost_limit_microunits=None, currency=None, require_hard_cost_enforcement=False))
        current = http(server + "/api/projects/" + PROJECT,
            dict(operation_id="e0000000-0000-4000-8000-000000000008", expected_revision=0, binding=binding))
    result = dict(designation=choice, project_id=PROJECT, task_id=TASK, workspace_id=WORKSPACE,
                  repository_id=REPOSITORY, node_id=node_id)
    receipt.write_text(json.dumps(result, sort_keys=True) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "seed", "settlement", "up", "down", "readiness"])
    parser.add_argument("--local-fixture", action="store_true", help="accept only a /fixture/ path owned by the disposable smoke test")
    args, remaining = parser.parse_known_args()
    try:
        if args.command == "seed":
            result = seed(args.local_fixture)
            result["settlement"] = enable_settlement()
        elif args.command == "settlement":
            result = enable_settlement()
        else:
            result = prepare()
            if args.command == "up":
                candidate = json.loads((ROOT / "vendor/worker-candidate.json").read_text())
                result.update(AQ_WORKER_SOURCE_REVISION=candidate["source_revision"], AQ_WORKER_ARCHIVE_SHA256=candidate["archive_sha256"], AQ_WORKER_VERSION=candidate["version"])
                builds = []
                for variable, service in (("AQ_SERVER_IMAGE", "server"), ("AQ_DEV_GRAPH_IMAGE", "aq-graph")):
                    if not os.environ.get(variable):
                        builds.append(service)
                if builds:
                    compose(result, "build", *builds, capture=False)
                compose(result, "up", "--no-build", *remaining, capture=False)
                return
            if args.command == "down":
                compose(result, "down", *remaining, capture=False)
                return
            if args.command == "readiness":
                compose(result, "exec", "-T", "server", "aq-server", "dependency-readiness", "--vault", "/vault", capture=False)
                return
        print(json.dumps(result, sort_keys=True))
    except (ValueError, RuntimeError, OSError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
