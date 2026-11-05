#!/usr/bin/env python3
"""Disposable E0 acceptance: real Compose peers and observed crash recovery."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid
import urllib.error
import urllib.request

spec = importlib.util.spec_from_file_location("dev_stack", Path(__file__).with_name("dev-stack.py"))
dev = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dev)


def wait_for(check, description, seconds=120):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError("timed out: " + description)


def compose_dev_reaches_ready_with_graph_and_git():
    with tempfile.TemporaryDirectory(prefix="aq-e0-") as temp:
        directory = Path(temp)
        if "AQ_SERVER_IMAGE" not in os.environ:
            os.environ["AQ_SERVER_IMAGE"] = "aq-e0-server:" + uuid.uuid4().hex[:10]
        os.environ.update(AQ_DEV_STATE_DIR=str(directory / "state"), AQ_SECRETS_DIR=str(directory / "secrets"),
            COMPOSE_PROJECT_NAME="aq-e0-" + uuid.uuid4().hex[:10], AQ_DEV_ADMIN_PORT="0", AQ_DEV_WORKER_PORT="0",
            AQ_DEV_GRAPH_PORT="0")
        os.environ.pop("AQ_VAULT_DIR", None)
        env = dev.prepare()
        config = json.loads(dev.compose(env, "config", "--format", "json"))
        for service in ("postgres", "server", "aq-graph", "aq-git"):
            assert service in config["services"], "missing development service: " + service
            assert config["services"][service].get("healthcheck"), "missing healthcheck: " + service
        assert "cloudflared" not in config["services"], "tunnel enabled by default"
        for service in ("server", "aq-git"):
            assert config["services"][service]["user"] == "10001:10001", "runtime UID must be 10001: " + service
        assert config["services"]["aq-git"]["image"] == config["services"]["server"]["image"], "Git must run from the server image"
        for target in ("/var/lib/aq", "/run/aq-git"):
            mounts = [{volume["target"]: volume["source"] for volume in config["services"][service]["volumes"]}
                      for service in ("server", "aq-git")]
            assert target in mounts[0] and mounts[0].get(target) == mounts[1].get(target), "missing shared Git path: " + target
        started = False
        evidence = dict(test="compose_dev_reaches_ready_with_graph_and_git")
        try:
            if os.environ.get("AQ_DEV_SMOKE_NO_BUILD") != "1":
                # A preloaded immutable Graph image may be selected explicitly.
                builds = ["server"]
                if not os.environ.get("AQ_DEV_GRAPH_IMAGE"):
                    builds.append("aq-graph")
                dev.compose(env, "build", *builds, capture=False)
            started = True
            dev.compose(env, "up", "-d", "--no-build", "postgres", "graph-db-init", "aq-graph", "aq-git", "server")

            def healthy():
                rows = [json.loads(line) for line in dev.compose(env, "ps", "--format", "json").splitlines() if line]
                return rows if all(any(r["Service"] == service and r.get("Health") == "healthy" for r in rows)
                                   for service in ("postgres", "server", "aq-graph", "aq-git")) else None

            rows = wait_for(healthy, "all four services healthy")
            evidence["runtime_users"] = {service: dev.compose(env, "exec", "-T", service, "id", "-u").strip()
                                         for service in ("server", "aq-git")}
            assert all(user == "10001" for user in evidence["runtime_users"].values())
            ownership = dev.compose(env, "exec", "-T", "aq-git", "stat", "-c", "%u:%g:%a",
                "/var/lib/aq/git", "/var/lib/aq/source-staging", "/run/aq-git").splitlines()
            assert ownership == ["10001:10001:700"] * 3, ownership
            evidence["private_directory_ownership"] = ownership
            evidence["images"] = {r["Service"]: dev.compose(env, "images", "-q", r["Service"]).strip() for r in rows}
            def sql(query):
                return dev.compose(env, "exec", "-T", "postgres", "sh", "-ec",
                    'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; exec psql -U agent_q -d agent_q -tAc "$1"', "sh", query).strip()
            graph_role = sql(
                "SELECT r.rolname || ':' || r.rolsuper::text FROM pg_database d JOIN pg_roles r ON r.oid=d.datdba WHERE d.datname='aq_graph'").strip()
            assert graph_role == "aq_graph:false", "Graph database must have its own non-superuser role: " + graph_role
            evidence["graph_database_owner"] = graph_role
            def readiness():
                # Dependency readiness is accessible only from inside the server.
                result = subprocess.run(["docker", "compose", "-f", str(dev.ROOT / "compose.yaml"), "-f",
                    str(dev.ROOT / "compose.dev.yaml"), "exec", "-T", "server", "aq-server", "dependency-readiness", "--vault", "/vault"],
                    env=dict(os.environ, **env), capture_output=True, text=True, timeout=10)
                try:
                    return result.returncode, json.loads(result.stdout)
                except json.JSONDecodeError:
                    return result.returncode, {}

            code, ready = readiness()
            assert code == 0 and ready["dependencies"] == {"graph": "ready", "aq_git": "ready"}, ready
            evidence["initial_readiness"] = ready
            def port(service, internal):
                return dev.compose(env, "port", service, str(internal)).strip().rsplit(":", 1)[1]
            os.environ["AQ_DEV_GRAPH_URL"] = "http://127.0.0.1:" + port("aq-graph", 8741)
            os.environ["AQ_DEV_SERVER_URL"] = "http://127.0.0.1:" + port("server", 8080)
            os.environ["AQ_DEV_WORKER_URL"] = "http://127.0.0.1:" + port("server", 8081)
            # Keep the assigned ephemeral ports stable across activation restarts.
            os.environ["AQ_DEV_ADMIN_PORT"] = port("server", 8080)
            os.environ["AQ_DEV_WORKER_PORT"] = port("server", 8081)
            # Only our disposable local repository is made available to Git.
            fixture = directory / "state/fixture"
            fixture.mkdir(exist_ok=True)
            subprocess.run(["git", "init", "--bare", "--initial-branch=main", str(fixture / "scratch.git")], check=True, capture_output=True)
            os.environ.update(DEV_SCRATCH_REPO_URL="/fixture/scratch.git", DEV_SCRATCH_BASE_BRANCH="main", DEV_SCRATCH_CREDENTIAL_REF="none")
            seed = dev.seed(local_fixture=True)
            assert dev.seed(local_fixture=True) == seed, "seed replay changed identities"
            credential_path = directory / "secrets/dev_worker.json"
            saved_credential = credential_path.read_bytes()
            mismatched = json.loads(saved_credential)
            mismatched["node_id"] = str(uuid.uuid4())
            invalid = json.loads(saved_credential)
            invalid["credential"] = "aqn_" + "0" * 64
            for label, content in (("missing", None), ("mismatched", json.dumps(mismatched).encode()),
                                   ("invalid", json.dumps(invalid).encode()), ("truncated", b"{")):
                credential_path.unlink()
                if content is not None:
                    credential_path.write_bytes(content)
                    credential_path.chmod(0o600)
                try:
                    dev.seed(local_fixture=True)
                except ValueError as error:
                    assert "worker credential" in str(error), str(error)
                else:
                    raise AssertionError("seed accepted " + label + " worker credential")
                finally:
                    credential_path.write_bytes(saved_credential)
                    credential_path.chmod(0o600)
            assert dev.seed(local_fixture=True) == seed, "credential refusal changed seed identities"
            evidence["credential_replay_refusals"] = ["missing", "mismatched", "invalid", "truncated"]
            os.environ["DEV_SCRATCH_BASE_BRANCH"] = "changed-input"
            try:
                dev.seed(local_fixture=True)
            except ValueError as error:
                assert "designation changed" in str(error)
            else:
                raise AssertionError("changed designation replaced the existing project")
            finally:
                os.environ["DEV_SCRATCH_BASE_BRANCH"] = "main"
            project = dev.http(os.environ["AQ_DEV_SERVER_URL"] + "/api/projects/" + dev.PROJECT)
            assert project["revision"] == 1, project
            assert project["binding"]["allowed_node_ids"] == [seed["node_id"]]
            repository = dev.git_request(directory / "state", "repo.show")
            assert repository["remote_url"] == "/fixture/scratch.git"
            assert repository["allowed_refs"] == ["refs/heads/main"]
            assert len(dev.http(os.environ["AQ_DEV_SERVER_URL"] + "/api/workers")) == 1
            node = dev.http(os.environ["AQ_DEV_GRAPH_URL"] + "/v1/workspaces/" + dev.WORKSPACE + "/nodes/" + dev.TASK,
                            token=(directory / "secrets/graph_token").read_text())
            assert node["id"] == dev.TASK and node["kind"] == "task", node
            evidence["seed"] = seed
            settlement = dev.enable_settlement()
            assert dev.enable_settlement() == settlement, "settlement restart changed retained pins"
            evidence["settlement"] = settlement
            socket = dev.compose(env, "exec", "-T", "server", "stat", "-c", "%u:%a:%F",
                "/run/aq-postgresql", "/run/aq-postgresql/.s.PGSQL.5432").splitlines()
            assert socket[0].endswith(":755:directory") and socket[1].endswith(":socket"), socket
            evidence["postgres_socket"] = socket
            engine_role = sql("SELECT r.rolname || ':' || r.rolsuper::text FROM pg_namespace n JOIN pg_roles r ON r.oid=n.nspowner WHERE n.nspname='aq_playbooks'")
            assert engine_role == "aq_playbook:false", engine_role
            evidence["engine_schema_owner"] = engine_role
            private_paths = ["/run/aq-dev/" + name for name in ("playbook_dsn", "playbook_engine_keys",
                "playbook_bridge_key", "playbook-artifact.json")]
            ownership = dev.compose(env, "exec", "-T", "server", "stat", "-c", "%u:%g:%a:%h", *private_paths).splitlines()
            assert ownership == ["10001:10001:600:1"] * 4, ownership
            evidence["settlement_private_files"] = ownership
            inbox = dev.http(os.environ["AQ_DEV_SERVER_URL"] + "/api/reviews/decisions")
            assert inbox["api_version"] == "review-inbox-1" and inbox["items"] == [], inbox
            evidence["decision_inbox"] = inbox
            # A fresh stack has no report/completion to settle. Require the typed
            # owner's refusal, distinguishing it from an unmounted route's 404.
            reconcile = os.environ["AQ_DEV_SERVER_URL"] + "/api/settlement/completions/" + str(uuid.uuid4()) + "/reconcile"
            for payload, expected, message in (({}, 404, "completion not found"),
                    ([], 400, "reconcile requires an empty JSON object")):
                request = urllib.request.Request(reconcile, data=json.dumps(payload).encode(),
                    headers={"Content-Type": "application/json", "X-AQ-Request": "1"})
                try:
                    urllib.request.urlopen(request, timeout=15)
                except urllib.error.HTTPError as error:
                    body = json.load(error)
                    assert error.code == expected and body["message"] == message, body
                else:
                    raise AssertionError("fresh stack accepted an absent completion")
            evidence["reconcile"] = "mounted owner: absent completion refused; non-object body refused"
            assert sql("SELECT (SELECT count(*) FROM human_decision_requests) + (SELECT count(*) FROM profiled_task_completions) + (SELECT count(*) FROM playbook_admissions)") == "0"
            def git_pid():
                return dev.compose(env, "exec", "-T", "aq-git", "cat", "/run/aq-git/child.pid").strip()
            before = git_pid()
            killed_at = time.monotonic()
            # aq-git forks a guarded engine. Kill serve, leaving its guard to
            # reap descendants and mark state clean before supervised restart.
            evidence["killed_engine_pid"] = dev.compose(env, "exec", "-T", "aq-git", "sh", "-ec",
                'guard=$(cat /run/aq-git/child.pid); children=$(cat /proc/$guard/task/$guard/children); engine=""; for pid in $children; do if [ "$(cat /proc/$pid/comm)" = aq-git ]; then test -z "$engine"; engine=$pid; fi; done; test -n "$engine"; kill -KILL "$engine"; printf "%s\\n" "$engine"').strip()
            attempts = []
            def degraded():
                code, report = readiness()
                attempts.append(dict(seconds=round(time.monotonic() - killed_at, 2), code=code, report=report,
                    pid=git_pid()))
                return report if code != 0 and report.get("dependencies", {}).get("aq_git") == "degraded" else None
            try:
                evidence["after_git_kill"] = wait_for(degraded, "Git degradation after killing serve", seconds=10)
            except AssertionError:
                print(json.dumps(dict(git_kill_before_pid=before, attempts=attempts)), flush=True)
                raise
            def recovered():
                code, report = readiness()
                after = git_pid()
                return report if after != before and code == 0 and report.get("dependencies", {}).get("aq_git") == "ready" else None
            evidence["after_git_restart"] = wait_for(recovered, "new Git process and ready report")
            dev.compose(env, "stop", "aq-graph")
            code, graph_down = readiness()
            assert code != 0 and graph_down["dependencies"]["graph"] == "degraded", graph_down
            dev.compose(env, "start", "aq-graph")
            wait_for(lambda: readiness()[0] == 0 and healthy(), "graph recovery")
            evidence["graph_outage_degraded"] = graph_down
        except Exception:
            # Retain diagnostics before removing this smoke's disposable peers.
            print(dev.compose(env, "logs", "--no-color", "--tail", "60", "postgres",
                "playbook-db-init", "dev-state-init", "server"), file=sys.stderr)
            raise
        finally:
            if started:
                dev.compose(env, "down", "--volumes", "--remove-orphans")
                assert not dev.compose(env, "ps", "-aq").strip(), "owned containers survived cleanup"
        evidence["cleanup"] = "owned Compose containers and volumes removed"
        print(json.dumps(evidence, sort_keys=True))
        print("PASS compose_dev_reaches_ready_with_graph_and_git")


if __name__ == "__main__":
    scripted = {"prod": "compose-prod-smoke.py", "prod-workers": "compose-workers-smoke.py",
                "prod-boat": "compose-boat-smoke.py"}
    if len(sys.argv) == 2 and sys.argv[1] in scripted:
        import runpy
        runpy.run_path(str(Path(__file__).with_name(scripted[sys.argv[1]])), run_name="__main__")
        sys.exit(0)
    if sys.argv[1:] != ["dev"]:
        raise SystemExit("usage: scripts/compose-smoke.sh dev|prod|prod-workers|prod-boat")
    compose_dev_reaches_ready_with_graph_and_git()
