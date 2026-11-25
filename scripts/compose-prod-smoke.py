#!/usr/bin/env python3
"""Disposable production Compose acceptance; Access auth remains enabled."""
import contextlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile
import time
import uuid

from migration_catalog import state_format

ROOT = Path(__file__).resolve().parents[1]
DIGEST = re.compile(r".+@sha256:[0-9a-f]{64}$")


def validate(config):
    services = config["services"]
    for name in ("postgres", "server", "aq-graph", "aq-git"):
        service = services[name]
        assert DIGEST.fullmatch(service["image"]), "unpinned image: " + name
        assert service.get("healthcheck"), "missing healthcheck: " + name
        assert not service.get("ports"), "host ports on " + name
        assert service["read_only"] and service["cap_drop"] == ["ALL"]
        assert "no-new-privileges:true" in service["security_opt"]
        assert service["logging"]["options"] == {"max-size": "10m", "max-file": "3"}
    assert config["networks"]["database"]["internal"]
    assert services["server"]["environment"]["AQ_DEPLOYMENT"] == "production"
    assert services["server"]["depends_on"]["aq-git"]["condition"] == "service_healthy"
    assert services["aq-git"]["restart"] in ("on-failure", "unless-stopped")
    assert services["aq-git"]["image"] == services["server"]["image"]
    assert "cloudflared" not in services, "smoke must disable only the tunnel"
    for target in ("/var/lib/aq", "/run/aq-git"):
        sources = [{v["target"]: v["source"] for v in services[name]["volumes"]}
                   for name in ("server", "aq-git")]
        assert sources[0][target] == sources[1][target]


class Stack:
    """Disposable production stack and the resources it owns."""

    def __init__(self, project, directory, env, compose_files):
        self.project = project
        self.directory = directory
        self.env = env
        self.compose_files = compose_files
        self.registry = project + "-registry"
        self.created_registry = False
        self.created_image = None
        self.started = False

    def run(self, *args, check=True):
        result = subprocess.run(args, cwd=ROOT, env=self.env, capture_output=True, text=True, timeout=1800)
        if check and result.returncode:
            raise RuntimeError("command failed: " + " ".join(args) + "\n" + result.stdout + result.stderr)
        return result

    def compose(self, *args, check=True):
        files = ["-f", str(ROOT / "compose.yaml")]
        for path in self.compose_files:
            files += ["-f", str(ROOT / path)]
        return self.run("docker", "compose", "-p", self.project, *files,
                        "-f", str(self.directory / "smoke.json"), *args, check=check)

    @staticmethod
    def wait(check, description, seconds=120):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = check()
            if value:
                return value
            time.sleep(0.5)
        raise AssertionError("timed out: " + description)

    def readiness(self):
        result = self.compose("exec", "-T", "server", "aq-server", "dependency-readiness",
                              "--vault", "/vault", check=False)
        return result.returncode, json.loads(result.stdout or "{}")

    def prepare_image(self):
        # Use the engine host's loopback for the disposable registry.
        address = "127.0.0.1:" + str(10000 + secrets.randbelow(50000))
        self.run("docker", "run", "-d", "--name", self.registry, "--network", "host",
                 "-e", "REGISTRY_HTTP_ADDR=" + address,
                 "registry:2@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373")
        self.created_registry = True
        self.wait(self.registry_ready, "disposable registry", 15)
        self.created_image = address + "/aq-server:" + self.project
        print("Building production smoke image", flush=True)
        self.run("docker", "build", "--target", "server", "--build-arg", "AQ_STATE_FORMAT=" + str(state_format(ROOT)),
                 "-t", self.created_image, ".")
        # The local registry uses an isolated anonymous Docker client.
        client_config = self.directory / "docker-local"
        client_config.mkdir()
        (client_config / "config.json").write_text('{"auths": {}}')
        self.run("docker", "--config", str(client_config), "push", self.created_image)
        inspected = self.run("docker", "image", "inspect", self.created_image)
        self.env["AQ_SERVER_IMAGE"] = json.loads(inspected.stdout)[0]["RepoDigests"][0]

    def registry_ready(self):
        logs = self.run("docker", "logs", self.registry)
        return "listening on" in logs.stdout + logs.stderr

    def write_fixture(self):
        vault = ROOT / "vault.production.example/config.toml"
        (self.directory / "vault/config.toml").write_text(vault.read_text())
        (self.directory / "smoke.json").write_text(json.dumps({
            "services": {"cloudflared": {"profiles": ["tunnel"]}},
        }))

    def start(self, evidence):
        config = json.loads(self.compose("config", "--format", "json").stdout)
        validate(config)
        self.config = config
        evidence["images"] = {name: service["image"] for name, service in self.config["services"].items()}
        self.started = True
        self.compose("up", "-d", "--no-build", "--wait", "--wait-timeout", "120")

    def clean_compose(self):
        errors = []
        # Profiled services are known to Compose and need an explicit down.
        if self.compose("--profile", "*", "down", "--volumes", "--remove-orphans", check=False).returncode:
            errors.append("Compose down failed")
        if self.compose("ps", "-aq", check=False).stdout.strip():
            errors.append("owned containers survived cleanup")
        volumes = self.run("docker", "volume", "ls", "-q",
                           "--filter", "label=com.docker.compose.project=" + self.project)
        if volumes.stdout.strip():
            errors.append("owned volumes survived cleanup")
        return errors

    def cleanup(self):
        errors = self.clean_compose() if self.started else []
        if self.created_registry and self.run("docker", "rm", "-f", "-v", self.registry, check=False).returncode:
            errors.append("registry cleanup failed")
        if self.created_image and self.run("docker", "image", "rm", self.created_image, check=False).returncode:
            errors.append("temporary image cleanup failed")
        assert not errors, errors


def smoke_environment(directory, env):
    for path in ("vault", "secrets", "spool/requests", "spool/status"):
        (directory / path).mkdir(parents=True, exist_ok=True)
    for name in ("postgres_password", "server_database_password", "graph_database_password", "graph_token", "tunnel_token"):
        path = directory / "secrets" / name
        path.write_text(secrets.token_hex(32))
        path.chmod(0o644)  # Bind-mounted secrets are read by container UIDs.
    env.update(AQ_VAULT_DIR=str(directory / "vault"), AQ_SECRETS_DIR=str(directory / "secrets"),
               AQ_UPDATER_SPOOL=str(directory / "spool"))


@contextlib.contextmanager
def production_stack(prefix, evidence, *, compose_files=(), setup=None):
    """Prepare the pinned image and start a disposable production Compose project."""
    project = prefix + uuid.uuid4().hex[:12]
    env = os.environ.copy()
    env.pop("COMPOSE_PROFILES", None)
    env.pop("COMPOSE_FILE", None)
    evidence.update(
        project=project,
        source_revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
    )
    with tempfile.TemporaryDirectory(prefix=project) as temporary:
        directory = Path(temporary)
        smoke_environment(directory, env)
        supplied = env.get("AQ_SERVER_IMAGE")
        if supplied:
            assert DIGEST.fullmatch(supplied), "AQ_SERVER_IMAGE must include @sha256:..."
        stack = Stack(project, directory, env, compose_files)
        try:
            if not supplied:
                stack.prepare_image()
            stack.write_fixture()
            if setup:
                setup(stack)
            stack.start(evidence)
            yield stack
        except BaseException:
            if stack.started:
                print(stack.compose("--profile", "*", "logs", "--no-color", "--tail", "40", check=False).stdout)
            raise
        finally:
            stack.cleanup()
        evidence["cleanup"] = "owned Compose containers and volumes removed"


def compose_prod_reaches_ready_with_graph_and_git():
    evidence = {"test": "compose_prod_reaches_ready_with_graph_and_git"}
    with production_stack("aq-e1a-", evidence) as stack:
        compose, wait, readiness = stack.compose, stack.wait, stack.readiness
        code, ready = readiness()
        assert code == 0 and ready["dependencies"] == {"graph": "ready", "aq_git": "ready"}, ready
        evidence["initial_readiness"] = ready
        # The loopback updater readiness contract reports the live auth mode.
        auth = json.loads(compose("exec", "-T", "server", "aq-server", "readiness").stdout or "{}")
        assert auth["auth_mode"] == "cloudflare_access", auth
        evidence["auth_mode"] = auth["auth_mode"]
        ownership = compose("exec", "-T", "aq-graph", "stat", "-c", "%u:%g:%a", "/run/aq/graph_token").stdout.strip()
        assert ownership == "10001:10001:600", ownership
        evidence["graph_token_permissions"] = ownership
        checksum = compose("exec", "-T", "aq-git", "sha256sum", "/usr/local/bin/aq-git").stdout.split()[0]
        candidate = json.loads((ROOT / "vendor/git-candidate.json").read_text())
        assert checksum == candidate["binary_sha256"], "Git binary differs from A3b"
        evidence["aq_git_binary_sha256"] = checksum
        def sql(query):
            return compose("exec", "-T", "postgres", "sh", "-ec",
                'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; exec psql -h 127.0.0.1 -U agent_q -d agent_q -tAc "$1"',
                "sh", query).stdout.strip()
        owners = sql("SELECT d.datname || ':' || r.rolname || ':' || r.rolsuper FROM pg_database d JOIN pg_roles r ON r.oid=d.datdba WHERE d.datname IN ('aq_server','aq_graph') ORDER BY d.datname")
        assert owners.splitlines() == ["aq_graph:aq_graph:false", "aq_server:aq_server:false"], owners
        evidence["database_owners"] = owners.splitlines()
        assert sql("SELECT has_database_privilege('aq_server','aq_graph','CONNECT') || ':' || has_database_privilege('aq_graph','aq_server','CONNECT')") == "false:false"
        for initializer in ("graph-db-init", "server-db-init"):
            compose("run", "--rm", "--no-deps", initializer)
        sql("ALTER ROLE aq_graph CREATEDB")
        try:
            refused = compose("run", "--rm", "--no-deps", "graph-db-init", check=False)
            assert refused.returncode != 0 and "owner privileges differ" in refused.stdout + refused.stderr, (refused.returncode, refused.stdout, refused.stderr)
        finally:
            sql("ALTER ROLE aq_graph NOCREATEDB")
        evidence["database_init"] = "replay preserves owners; owner privilege drift refused; cross-owner CONNECT denied"
        for service, key in (("aq-git", "aq_git"), ("aq-graph", "graph")):
            compose("stop", service)
            code, degraded = readiness()
            assert code != 0 and degraded["dependencies"][key] == "degraded", degraded
            evidence[key + "_outage"] = degraded
            compose("start", service)
            wait(lambda: readiness()[0] == 0, service + " recovery")
        # Kill the serve engine and require the supervisor to replace it.
        before = compose("exec", "-T", "aq-git", "cat", "/run/aq-git/child.pid").stdout.strip()
        compose("exec", "-T", "aq-git", "sh", "-ec",
            'guard=$(cat /run/aq-git/child.pid); for pid in $(cat /proc/$guard/task/$guard/children); do if [ "$(cat /proc/$pid/comm)" = aq-git ]; then kill -KILL "$pid"; exit 0; fi; done; exit 1')
        wait(lambda: readiness()[1]["dependencies"]["aq_git"] == "degraded", "Git crash degradation", 10)
        def recovered():
            after = compose("exec", "-T", "aq-git", "cat", "/run/aq-git/child.pid").stdout.strip()
            return after != before and readiness()[0] == 0
        wait(recovered, "supervised Git recovery")
        evidence["git_crash_recovery"] = "new guard PID and dependencies ready"
    print(json.dumps(evidence, sort_keys=True))
    print("PASS compose_prod_reaches_ready_with_graph_and_git")


if __name__ == "__main__":
    compose_prod_reaches_ready_with_graph_and_git()
