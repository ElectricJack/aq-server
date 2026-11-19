#!/usr/bin/env python3
"""Disposable product assembly; communicate through public HTTP APIs only."""
import argparse
from datetime import datetime, timezone
import hashlib
from http.client import HTTPException
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import uuid

from migration_catalog import state_format as migration_state_format

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("worker_candidate", ROOT / "scripts/check-worker-candidate.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
# The foundation worker reaches only a dedicated rootless sandbox daemon, checked
# exactly as the production compose.workers.yaml smoke checks it.
workers_spec = importlib.util.spec_from_file_location("compose_workers_smoke", ROOT / "scripts/compose-workers-smoke.py")
workers_smoke = importlib.util.module_from_spec(workers_spec)
workers_spec.loader.exec_module(workers_smoke)
CONTROLLER_CONFIG = "/etc/aq-worker/controller.json"
# Locally built smoke images are never released: build them with the CI image
# profile (no ThinLTO, low optimization; Cargo.toml [profile.ci-image]).
IMAGE_PROFILE = os.environ.get("AQ_IMAGE_CARGO_PROFILE", "ci-image")
SANDBOX_SOCKET = "/run/aq-sandbox/podman.sock"


# This older product fixture exercises the server/worker foundation. Production
# graph/Git readiness is covered by compose-smoke.sh prod; keep its independent
# dev-auth fixture from inheriting production provisioning or credentials.
FOUNDATION_OVERRIDE = """services:
  server:
    depends_on: !override
      postgres: {condition: service_healthy}
    secrets: !override [postgres_password]
    command:
      - 'export PGPASSWORD="$$(cat /run/secrets/postgres_password)" DATABASE_URL="postgres://agent_q@postgres:5432/agent_q"; exec aq-server serve --vault /vault'
  aq-graph:
    profiles: [production]
  aq-git:
    profiles: [production]
  graph-db-init:
    profiles: [production]
  server-db-init:
    profiles: [production]
  prod-state-init:
    profiles: [production]
"""


def utc():
    return datetime.now(timezone.utc).isoformat()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


class Fixture:
    def __init__(self, execution):
        self.execution = execution
        self.project = "aq-product-" + uuid.uuid4().hex[:12]
        self.directory = tempfile.TemporaryDirectory(prefix=self.project + "-")
        self.temp = Path(self.directory.name)
        evidence = os.environ.get("AQ_SMOKE_EVIDENCE_DIR")
        self.logs = Path(evidence).resolve() if evidence else Path(tempfile.mkdtemp(prefix=self.project + "-evidence-"))
        self.logs.mkdir(parents=True, exist_ok=True)
        self.env = os.environ.copy()
        self.commands = []
        self.images = []
        self.runtime = None
        self.heartbeat_container = None
        self.dc = []
        self.record = {"fixture": "execution" if execution else "foundation", "project": self.project,
                       "started_at": utc(), "commands": self.commands, "scope": "local candidate; no hosted acceptance"}

    def say(self, value):
        print(f"smoke: {value}", flush=True)

    def run(self, command, *, name, input=None, check=True, timeout=240, secret_output=False):
        start = utc()
        result = subprocess.run(command, cwd=ROOT, env=self.env, input=input, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        self.commands.append({"command": command, "started_at": start, "finished_at": utc(),
                              "exit_code": result.returncode, "log": None if secret_output else name + ".log"})
        if not secret_output:
            (self.logs / (name + ".log")).write_text(result.stdout)
        if check and result.returncode:
            if not secret_output:
                print("\n".join(result.stdout.splitlines()[-35:]), file=sys.stderr)
            raise RuntimeError(f"{name} exited {result.returncode}")
        return result.stdout

    def compose(self, *args, name, **kwargs):
        return self.run([*self.dc, *args], name=name, **kwargs)

    def poll(self, description, call, timeout=90):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = call()
                if value:
                    return value
            except (OSError, ValueError, HTTPException, urllib.error.URLError, RuntimeError):
                pass
            time.sleep(0.5)
        raise RuntimeError(f"timed out waiting for {description}")

    def http(self, path, body=None, *, machine=False, credential=None, expected=200):
        url = (self.worker if machine else self.admin) + path
        headers = {"X-AQ-Request": "1"}
        if credential:
            headers["Authorization"] = "Bearer " + credential
        data = None if body is None else canonical(body)
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data, headers)
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        if status != expected:
            raise RuntimeError(f"HTTP {path}: got {status}, expected {expected}")
        return json.loads(raw) if raw else None

    def address(self):
        def mapped(port):
            output = self.compose("port", "server", str(port), name="address-" + str(port), check=False).strip()
            return output if re.fullmatch(r"127\.0\.0\.1:[0-9]+", output) else None
        self.admin = "http://" + self.poll("published admin address", lambda: mapped(8080))
        self.worker = "http://" + self.poll("published worker address", lambda: mapped(8081))

    def worker_row(self):
        rows = [row for row in self.http("/api/workers") if row["name"] == "smoke"]
        return rows[0] if len(rows) == 1 else None

    def config(self):
        # The disposable node credential is kept in memory and never in evidence.
        text = self.compose("exec", "-T", "worker", "cat",
                            "/config/agent-q/worker.toml", name="private-config-read", secret_output=True)
        return tomllib.loads(text)

    def setup(self):
        official = os.environ.get("AQ_SMOKE_OFFICIAL") == "1"
        candidate = checker.candidate(official)
        self.record["worker_candidate"] = candidate
        server = os.environ.get("AQ_SMOKE_SERVER_IMAGE")
        worker = os.environ.get("AQ_SMOKE_WORKER_IMAGE")
        # The synthetic-runtime harness is a separate CI-only image, never the
        # production worker; it is built locally from the same candidate unless given.
        fixture = os.environ.get("AQ_SMOKE_FIXTURE_IMAGE")
        if bool(server) != bool(worker):
            raise ValueError("set both AQ_SMOKE_SERVER_IMAGE and AQ_SMOKE_WORKER_IMAGE, or neither")
        if official and (not server or any(not re.fullmatch(r"[a-z0-9][a-z0-9./:_-]*@sha256:[0-9a-f]{64}", image)
                                          for image in (server, worker))):
            raise ValueError("official acceptance requires digest-pinned server and worker images")
        snapshot = self.temp / "source"
        snapshot.mkdir()
        for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "Dockerfile", "Dockerfile.worker",
                     ".dockerignore", "compose.yaml"]:
            shutil.copy2(ROOT / name, snapshot / name)
        for name in ["crates", "vendor", "protocol"]:
            shutil.copytree(ROOT / name, snapshot / name)
        # Dockerfile.worker installs the worker volume initializer from scripts/.
        (snapshot / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/worker-init-volumes.sh", snapshot / "scripts/worker-init-volumes.sh")
        inventory = {str(p.relative_to(snapshot)): sha(p.read_bytes()) for p in sorted(snapshot.rglob("*")) if p.is_file()}
        self.record["source_inventory"] = inventory
        self.record["source_tree_sha256"] = sha(canonical(inventory))
        self.record["source_revision"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        self.record["working_tree_dirty"] = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT))
        state = str(migration_state_format(snapshot))
        worker_args = ["--build-arg", "AQ_SOURCE_REVISION=" + candidate["source_revision"],
                       "--build-arg", "AQ_WORKER_ARCHIVE_SHA256=" + candidate["archive_sha256"],
                       "--build-arg", "AQ_VERSION=" + candidate["version"], "--build-arg", "AQ_STATE_FORMAT=" + state,
                       "--build-arg", "CARGO_PROFILE=" + IMAGE_PROFILE]
        if not fixture:
            fixture = self.project + "-fixture:candidate"
            self.images.append(fixture)
            self.say("build the synthetic-runtime fixture worker (never pushed)")
            self.run(["docker", "build", "-f", str(snapshot / "Dockerfile.worker"), "--target", "synthetic-runtime",
                      *worker_args, "-t", fixture, str(snapshot)], name="build-fixture", timeout=1800)
        if not server:
            server, worker = self.project + "-server:candidate", self.project + "-worker:candidate"
            self.images += [server, worker]
            revision = "dev-" + self.record["source_tree_sha256"][:24]
            self.say("build server and pinned standalone worker from a frozen source snapshot")
            self.record["image_cargo_profile"] = IMAGE_PROFILE
            self.run(["docker", "build", "--target", "server", "--build-arg", "AQ_SOURCE_REVISION=" + revision,
                      "--build-arg", "AQ_STATE_FORMAT=" + state, "--build-arg", "CARGO_PROFILE=" + IMAGE_PROFILE,
                      "-t", server, str(snapshot)], name="build-server", timeout=1800)
            self.run(["docker", "build", "-f", str(snapshot / "Dockerfile.worker"), "--target", "worker",
                      *worker_args, "-t", worker, str(snapshot)], name="build-worker", timeout=1800)
        else:
            for role, image in [("server", server), ("worker", worker)]:
                if subprocess.run(["docker", "image", "inspect", image], stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL).returncode:
                    self.run(["docker", "pull", image], name="pull-" + role, timeout=600)
        self.fixture_image = fixture
        self.record["images"] = json.loads(self.run(["docker", "image", "inspect", server, worker, fixture],
                                                    name="image-identities"))
        for role, image in [("worker", worker), ("fixture", fixture)]:
            embedded = json.loads(self.run(["docker", "run", "--rm", "--entrypoint", "cat", image,
                                            "/usr/share/agent-q/worker-candidate.json"], name=role + "-provenance"))
            if embedded != candidate:
                raise ValueError(role + " image provenance differs from selected immutable candidate")
            version = self.run(["docker", "run", "--rm", image, "--version"], name=role + "-version").strip()
            if version != "aq-worker " + candidate["version"]:
                raise ValueError(role + " executable version differs from selected candidate")
        help_text = self.run(["docker", "run", "--rm", fixture, "run-execution", "--help"], name="fixture-execution-cli")
        if "--drop-acceptance-response-once" not in help_text or "--drop-report-response-once" not in help_text:
            raise ValueError("fixture executable lacks the selected execution candidate interface")
        self.record["fixture_binary_sha256"] = self.run([
            "docker", "run", "--rm", "--entrypoint", "sha256sum", fixture,
            "/usr/local/bin/aq-worker", "/usr/local/bin/aq-fixture-agent"], name="fixture-binary-identities")
        self.record["worker_binary_sha256"] = self.run([
            "docker", "run", "--rm", "--entrypoint", "sha256sum", worker,
            "/usr/local/bin/aq-worker"], name="worker-binary-identities")
        # Recovery drives only its own fixture build; the product worker must
        # not ship the synthetic harness at all.
        if "recovery_build" not in self.record:
            shipped = self.run(["docker", "run", "--rm", worker, "run-execution", "--help"],
                               name="worker-has-no-execution-cli", check=False)
            if "--fixture-agent" in shipped or self.run(
                    ["docker", "run", "--rm", "--entrypoint", "sh", worker, "-c", "if test -e /usr/local/bin/aq-fixture-agent; then echo present; fi"],
                    name="worker-has-no-fixture-agent", check=False).strip():
                raise ValueError("production worker image ships the synthetic-runtime harness")
        git_candidate = json.loads((snapshot / "vendor/git-candidate.json").read_text())
        self.record["git_candidate"] = git_candidate
        self.record["git_runtime"] = {}
        # The recovery-only server intentionally omits product components.
        roles = [("worker", worker)] if "recovery_build" in self.record else [("server", server), ("worker", worker)]
        for role, image in roles:
            embedded_git = json.loads(self.run(["docker", "run", "--rm", "--entrypoint", "cat", image,
                "/usr/share/agent-q/git-candidate.json"], name=role + "-git-provenance"))
            if embedded_git != git_candidate:
                raise ValueError(role + " aq-git provenance differs from immutable candidate")
            git_version = self.run(["docker", "run", "--rm", "--entrypoint", "/usr/bin/git", image,
                "--version"], name=role + "-git-version").strip()
            if git_version != "git version " + git_candidate["git"]["version"]:
                raise ValueError(role + " default Git executable differs from pinned runtime")
            capabilities = json.loads(self.run(["docker", "run", "--rm", "--entrypoint", "aq-git", image,
                "--state-dir", "/tmp/aq-git-smoke", "--json", "capabilities"], name=role + "-git-capabilities"))
            assert capabilities["ok"] and capabilities["result"]["protocol_version"] == git_candidate["protocol_version"]
            assert capabilities["result"]["git"]["verification"]["status"] in ("unverified", "verified")
            binary = self.run(["docker", "run", "--rm", "--entrypoint", "sha256sum", image,
                "/usr/local/bin/aq-git", "/opt/aq-git-runtime/bin/git"], name=role + "-git-binary-identities")
            self.record["git_runtime"][role] = {"version": git_version, "binary_sha256": binary,
                                               "capabilities": capabilities}
        for name in ["vault", "secrets", "spool/requests", "spool/status"]:
            (self.temp / name).mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / "vault.example/config.toml", self.temp / "vault/config.toml")
        (self.temp / "vault").chmod(0o755)
        (self.temp / "secrets/postgres_password").write_text(secrets.token_hex(24))
        (self.temp / "secrets/tunnel_token").write_text("unused-disposable-fixture")
        for secret in (self.temp / "secrets").iterdir():
            secret.chmod(0o644)
        self.env.update(AQ_SERVER_IMAGE=server, AQ_WORKER_IMAGE=worker, AQ_VAULT_DIR=str(self.temp / "vault"),
                        AQ_SECRETS_DIR=str(self.temp / "secrets"), AQ_UPDATER_SPOOL=str(self.temp / "spool"))
        hardened = {"read_only": True, "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"]}
        volumes = ["worker_config:/config", "worker_state:/state"]
        worker_service = {"image": worker, "networks": ["edge"], "volumes": volumes, **hardened}
        services = {
            "server": {"environment": {"AQ_DEPLOYMENT": "development"}, "ports": ["127.0.0.1::8080", "127.0.0.1::8081"]},
            "cloudflared": {"profiles": ["tunnel"]},
            "worker": worker_service,
            # The synthetic harness shares the enrolled identity and state volumes
            # but runs from the CI-only fixture image.
            "runtime": {**worker_service, "image": fixture, "profiles": ["runtime"]}}
        if "recovery_build" not in self.record:
            # 0.5.1 `run` binds the docker-local backend, which accepts only a
            # dedicated rootless daemon socket owned by the worker's own UID.
            socket, sandbox = workers_smoke.sandbox_socket()
            owner = os.stat(socket)
            user = f"{owner.st_uid}:{owner.st_gid}"
            self.record["sandbox"] = dict(sandbox, uid=owner.st_uid, gid=owner.st_gid)
            controller = self.temp / "controller.json"
            controller.write_text(json.dumps({
                "state_dir": "/state/controller",
                "node": {"runtime": {"id": "aq.product-smoke", "version": "1", "digest": sha(b"product-smoke")},
                         "capabilities": [], "features": [], "templates": [], "max_concurrent_sandboxes": 1,
                         "spend_ceiling": None,
                         "capacity": {"memory_bytes": 1 << 30, "cpu_millis": 1000, "disk_bytes": 1 << 30,
                                      "max_preparing": 1}},
                "docker_local": {"socket": SANDBOX_SOCKET}}, indent=2))
            controller.chmod(0o644)
            for name in ["worker", "runtime"]:
                # nocopy: Docker copies an image VOLUME's content and owner into a volume
                # that is still empty on every mount, which would undo the init chown.
                services[name].update(user=user, volumes=[
                    *[volume + ":nocopy" for volume in volumes], os.path.dirname(socket) + ":" + os.path.dirname(SANDBOX_SOCKET),
                    str(controller) + ":" + CONTROLLER_CONFIG + ":ro"])
            services["worker"]["command"] = ["run", "--controller-config", CONTROLLER_CONFIG]
            # One-shot owner change of the image's 10001-owned volumes to the socket owner.
            services["worker-volume-init"] = {
                "image": worker, "user": "0:0", "entrypoint": ["chown", "-R", user, "/config", "/state"],
                "volumes": volumes, "network_mode": "none", "profiles": ["init"], **hardened,
                "cap_add": ["CHOWN", "DAC_OVERRIDE", "FOWNER"]}
        overlay = {"services": services, "volumes": {"worker_config": {}, "worker_state": {}}}
        file = self.temp / "compose.smoke.json"
        file.write_text(json.dumps(overlay))
        foundation = self.temp / "compose.foundation.yaml"
        foundation.write_text(FOUNDATION_OVERRIDE)
        self.dc = ["docker", "compose", "-p", self.project, "-f", str(snapshot / "compose.yaml"),
                   "-f", str(foundation), "-f", str(file)]
        if "worker-volume-init" in services:
            self.compose("run", "--rm", "-T", "--no-deps", "worker-volume-init", name="worker-volume-init")
        self.compose("up", "-d", "--no-build", "--wait", "--wait-timeout", "120", "server", name="start-server")
        self.address()
        self.poll("admin health", lambda: self.http("/healthz"))
        self.record["postgres_image"] = self.compose("images", "--format", "json", name="compose-images")

    def foundation(self):
        self.say("enroll the selected standalone worker and verify probe/compatibility")
        code = self.http("/api/enrollments", {}, expected=201)["code"]
        self.compose("run", "--rm", "-T", "--no-deps", "worker", "enroll", "--server", "http://server:8081",
                     "--name", "smoke", name="enroll", input=code + "\n")
        self.compose("up", "-d", "--no-build", "worker", name="start-worker")
        self.heartbeat_container = self.compose("ps", "-q", "worker", name="heartbeat-container").strip()
        assert re.fullmatch(r"[0-9a-f]{64}", self.heartbeat_container), "expected one heartbeat connector container"
        def online():
            row = self.worker_row()
            return row if row and row["status"] == "online" and row.get("last_seen_at") else None
        row = self.poll("first heartbeat", online)
        self.node = row["id"]
        saved = self.config()
        assert saved["node_id"] == self.node
        self.credential = saved["credential"]
        self.record["node_id"] = self.node
        response = self.http("/v1/worker/probe", {"nonce": "product-smoke"}, machine=True, credential=self.credential)
        assert response["nonce"] == "product-smoke"
        response = self.http("/v1/worker/heartbeat", {"worker_version": "fixture", "protocol_version": 99},
                             machine=True, credential=self.credential, expected=426)
        assert response["error"] == "unsupported_protocol" and response["supported_min"] == 1 and response["supported_max"] == 1
        execution_error = self.http("/v2/worker/register", {
            "identity": {"node_id": self.node, "boot_id": str(uuid.uuid4())},
            "operation_id": str(uuid.uuid4()), "protocol_version": 99,
            "worker_version": "fixture", "capabilities": []},
            machine=True, credential=self.credential, expected=426)
        assert execution_error["code"] == "unsupported_protocol"
        assert self.worker_row()["status"] == "online"
        self.say("restart daemon and PostgreSQL; preserve node identity and advance heartbeat")
        before = row["last_seen_at"]
        self.compose("restart", "server", "postgres", name="restart")
        self.address()
        self.poll("restarted health", lambda: self.http("/healthz"))
        def advanced():
            current = online()
            return current if current and current["id"] == self.node and current["last_seen_at"] > before else None
        after = self.poll("same-node reconnect", advanced)
        assert self.config() == saved
        self.record["foundation"] = {"first_heartbeat": before, "later_heartbeat": after["last_seen_at"],
                                     "identity_preserved": True, "probe": "passed", "unsupported_protocol": "passed"}
        self.record["foundation"]["unsupported_execution_preserves_connectivity"] = "passed"

    def execute(self):
        self.say("run synthetic assignment with lost acceptance and report acknowledgements")
        self.runtime = self.project + "-execution"
        self.compose("run", "-d", "--no-deps", "--name", self.runtime, "-e", "AQ_FIXTURE_HOLD=1", "runtime",
                     "run-execution", "--state-dir", "/state/runtime", "--fixture-agent", "/usr/local/bin/aq-fixture-agent",
                     "--idle-timeout-secs", "90", "--exit-after-result", "--drop-acceptance-response-once",
                     "--drop-report-response-once", name="start-execution")
        def registration():
            output = self.run(["docker", "logs", self.runtime], name="execution-log")
            for line in output.splitlines():
                try:
                    event = json.loads(line)
                    if event.get("event") == "registered":
                        return event["identity"]
                except ValueError:
                    pass
        identity = self.poll("execution registration", registration)
        assert identity["node_id"] == self.node
        attempt = {"attempt_id": str(uuid.uuid4()), "session_id": str(uuid.uuid4()), "generation": 1}
        spec = {"project_id": str(uuid.uuid4()), "harness": "deterministic-agent-v1", "fixture_output": "product synthetic success",
                "resources": {"slots": 1, "cpu_millis": 100, "memory_bytes": 1048576, "disk_bytes": 1048576},
                "required_capabilities": ["deterministic-agent-v1"]}
        spec["resolved_spec_digest"] = sha(canonical(spec))
        offer = {"offer_id": str(uuid.uuid4()), "identity": identity, "attempt": attempt, "spec": spec,
                 "offer_expires_at_ms": int(time.time() * 1000) + 90000, "reservation_ttl_ms": 90000}
        # Pause only this disposable runtime so cancellation definitely precedes
        # its first opportunity to fetch the cancelled offer.
        self.run(["docker", "pause", self.runtime], name="pause-before-offers")
        cancelled = dict(offer, offer_id=str(uuid.uuid4()), attempt={
            "attempt_id": str(uuid.uuid4()), "session_id": str(uuid.uuid4()), "generation": 1})
        try:
            self.http("/api/execution/fixtures", {"task_id": str(uuid.uuid4()), "offer": cancelled})
            cancelled_state = self.http("/api/execution/attempts/" + cancelled["attempt"]["attempt_id"] + "/cancel",
                                        {"operation_id": str(uuid.uuid4()), "reason": "cancel before worker sees offer"})
            assert cancelled_state["status"] == "declined"
            self.http("/api/execution/fixtures", {"task_id": str(uuid.uuid4()), "offer": offer})
        finally:
            self.run(["docker", "unpause", self.runtime], name="unpause-after-offers", check=False)
        def claim():
            value = self.compose("run", "--rm", "-T", "--no-deps", "--entrypoint", "sh", "runtime", "-ec",
                                 "find /state/runtime/launches -name started.claim -print", name="launch-claim", check=False)
            rows = [line for line in value.splitlines() if re.fullmatch(r"/state/runtime/launches/[0-9a-f-]{36}/started.claim", line)]
            if rows:
                assert len(rows) == 1, "duplicate synthetic process launch"
                return rows[0]
        started = self.poll("one durable process launch", claim)
        launch_id = started.split("/")[-2]
        self.run(["docker", "exec", self.runtime, "touch", started.rsplit("/", 1)[0] + "/release"], name="release-fixture")
        def exited():
            state = json.loads(self.run(["docker", "inspect", "--format", "{{json .State}}", self.runtime], name="execution-state"))
            if state["Status"] == "exited":
                assert state["ExitCode"] == 0, "standalone execution worker failed"
                return state
        self.poll("execution report acknowledgement", exited)
        self.run(["docker", "logs", self.runtime], name="execution-log")
        state = self.http("/api/execution/attempts/" + attempt["attempt_id"])
        assert state["status"] == "finished" and state["stopped"]
        assert state["disposition"]["disposition"] == "accepted"
        assert state["report"]["output"] == spec["fixture_output"]
        text = self.compose("run", "--rm", "-T", "--no-deps", "--entrypoint", "cat", "runtime",
                            "/state/runtime/journal.json", name="execution-journal")
        journal = json.loads(text[text.index("{"):])
        assert len(journal["attempts"]) == 1
        local = journal["attempts"][attempt["attempt_id"]]
        assert local["launch_id"] == launch_id and local["phase"] == "retired"
        assert local["result"]["report_id"] == state["disposition"]["report_id"]
        replay = self.http("/v2/worker/result", state["report"], machine=True, credential=self.credential)
        assert replay == state["disposition"]
        self.compose("restart", "server", name="restart-after-result")
        self.address()
        recovered = self.poll("durable accepted result", lambda: self.http("/api/execution/attempts/" + attempt["attempt_id"]))
        assert recovered == state
        self.record["execution"] = {"attempt_id": attempt["attempt_id"], "boot_id": identity["boot_id"],
                                    "launch_id": launch_id, "launch_count": 1, "result": state,
                                    "acceptance_response_loss": "passed", "report_response_loss": "passed",
                                    "report_replay": "passed", "daemon_restart": "passed", "graph_completion": "not exercised"}
        self.record["execution"]["cancelled_unaccepted_attempt_id"] = cancelled["attempt"]["attempt_id"]
        self.record["execution"]["cancel_before_poll_then_acquire"] = "passed"

    def revoke(self):
        self.say("revoke credential; verify machine rejection and connector exit")
        self.http("/api/workers/" + self.node + "/revoke", {}, expected=204)
        response = self.http("/v1/worker/probe", {"nonce": "revoked"}, machine=True, credential=self.credential, expected=401)
        assert response["error"] == "credential_revoked"
        container = self.heartbeat_container
        def stopped():
            state = json.loads(self.run(["docker", "inspect", "--format", "{{json .State}}", container], name="revoked-worker-state"))
            return state if state["Status"] == "exited" else None
        state = self.poll("revoked worker exit", stopped)
        assert state["ExitCode"] == 3
        self.record["foundation"]["revocation"] = "passed (HTTP 401 and connector exit 3)"

    def cleanup(self):
        errors = []
        confirmed = True

        def attempt(action):
            try:
                return action()
            except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                errors.append(str(error))

        try:
            if self.dc:
                attempt(lambda: self.compose("logs", "--no-color", "--tail", "100", name="stack-logs", check=False))
                attempt(lambda: self.compose("down", "-v", "--remove-orphans", name="teardown", check=False))
            if self.runtime:
                attempt(lambda: self.run(["docker", "rm", "-fv", self.runtime], name="runtime-teardown", check=False))
            # Each operation proceeds even when Compose or log collection fails.
            # Selection is restricted to this fixture's random project label.
            for kind, listing, removal in [
                ("containers", ["docker", "ps", "-aq"], ["docker", "rm", "-fv"]),
                ("volumes", ["docker", "volume", "ls", "-q"], ["docker", "volume", "rm"]),
                ("networks", ["docker", "network", "ls", "-q"], ["docker", "network", "rm"]),
            ]:
                command = [*listing, "--filter", "label=com.docker.compose.project=" + self.project]
                try:
                    ids = self.run(command, name="remaining-" + kind).split()
                    if ids:
                        self.run([*removal, *ids], name="fallback-remove-" + kind)
                    if self.run(command, name="verified-" + kind).strip():
                        confirmed = False
                except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                    errors.append(str(error))
                    confirmed = False
            if self.images:
                attempt(lambda: self.run(["docker", "image", "rm", *self.images], name="image-teardown", check=False))
        finally:
            try:
                self.directory.cleanup()
            except OSError as error:
                errors.append(str(error))
            self.record["cleanup"] = {"owned_resources_removed": confirmed, "errors": errors}
            if not confirmed and self.record.get("outcome") == "passed":
                self.record["outcome"] = "failed"
                self.record["error"] = "owned resource cleanup could not be confirmed"
            self.record["finished_at"] = utc()
            (self.logs / "evidence.json").write_text(json.dumps(self.record, indent=2) + "\n")
            self.say("evidence " + str(self.logs))
        return confirmed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execution", action="store_true")
    args = parser.parse_args()
    fixture = Fixture(args.execution)
    status = 1
    try:
        fixture.setup()
        fixture.foundation()
        if args.execution:
            fixture.execute()
        fixture.revoke()
        fixture.record["outcome"] = "passed"
        status = 0
    except (OSError, ValueError, HTTPException, AssertionError, RuntimeError, subprocess.SubprocessError) as error:
        fixture.record["outcome"] = "failed"
        fixture.record["error"] = str(error)
        print(f"smoke: FAIL: {error}", file=sys.stderr)
    finally:
        if not fixture.cleanup():
            status = 1
    if status == 0:
        fixture.say("PASS")
    return status


if __name__ == "__main__":
    sys.exit(main())
