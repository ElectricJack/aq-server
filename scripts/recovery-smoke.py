#!/usr/bin/env python3
"""M3 real-process recovery fixture. Never treats PID absence as writer-stop proof."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import gzip
from http.client import HTTPException
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import subprocess
import sys
import tarfile
import time
import tomllib
import traceback
import uuid

from migration_catalog import state_format as migration_state_format

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("product_smoke", ROOT / "scripts/product-smoke.py")
product = importlib.util.module_from_spec(spec)
spec.loader.exec_module(product)


class RecoveryFixture(product.Fixture):
    def __init__(self, prototype_revision=None, prototype_repository=None):
        if os.environ.get("AQ_RECOVERY_EVIDENCE_DIR"):
            os.environ["AQ_SMOKE_EVIDENCE_DIR"] = os.environ["AQ_RECOVERY_EVIDENCE_DIR"]
        super().__init__(True)
        self.case = "assembly"
        self.project = self.project.replace("aq-product-", "aq-recovery-", 1)
        self.record.update(fixture="M3-distributed-recovery", project=self.project,
                           gate_status="not accepted; matrix must pass supported profile", cases=[])
        self.control = self.temp / "fault-control"
        self.control.mkdir(mode=0o755)
        self.case_data = None
        self.prototype_revision = prototype_revision
        self.prototype_repository = prototype_repository
        self.record["invocation"] = sys.argv
        harness = self.logs / "harness"
        harness.mkdir()
        for name in ["recovery-smoke.py", "recovery-smoke-proxy.py", "recovery-smoke-server.Dockerfile", "product-smoke.py", "check-worker-candidate.py", "migration_catalog.py"]:
            shutil.copy2(ROOT / "scripts" / name, harness / name)
        self.record["harness_sha256"] = {p.name: product.sha(p.read_bytes()) for p in harness.iterdir()}

    def run(self, command, *, name, **kwargs):
        if command[:2] == ["docker", "run"] and "com.docker.compose.project=" + self.project not in command:
            command = [*command[:2], "--label", "com.docker.compose.project=" + self.project, *command[2:]]
        return super().run(command, name=self.case + "-" + name, **kwargs)

    def setup(self):
        # Build the explicit test-feature server separately; the production
        # Dockerfile and release path never acquire recovery fixture controls.
        source = self.temp / "recovery-source"
        source.mkdir()
        for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "Dockerfile.worker", ".dockerignore"]:
            shutil.copy2(ROOT / name, source / name)
        for name in ["crates", "vendor", "protocol"]:
            shutil.copytree(ROOT / name, source / name)
        # Dockerfile.worker installs the worker volume initializer from scripts/.
        (source / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/worker-init-volumes.sh", source / "scripts/worker-init-volumes.sh")
        if self.prototype_revision:
            raw = subprocess.check_output(["git", "archive", "--format=tar", self.prototype_revision], cwd=self.prototype_repository)
            compressed = gzip.compress(raw, mtime=0)
            with tarfile.open(fileobj=io.BytesIO(raw)) as tar:
                package = tomllib.loads(tar.extractfile("crates/aq-worker/Cargo.toml").read().decode())["package"]
                lock = tar.extractfile("Cargo.lock").read()
                worker_state_format = int(re.search(r"pub const STATE_FORMAT: u32 = (\d+);", tar.extractfile("crates/aq-worker/src/journal.rs").read().decode()).group(1))
            candidate = json.loads((source / "vendor/worker-candidate.json").read_text())
            candidate.update(source_revision=self.prototype_revision, version=package["version"],
                             archive_sha256=product.sha(compressed), lockfile_sha256=product.sha(lock),
                             worker_journal_format=worker_state_format, distribution="PROTOTYPE recovery fixture only; not a qualified release")
            (source / "vendor/aq-worker-source.tar.gz").write_bytes(compressed)
            (source / "vendor/worker-candidate.json").write_text(json.dumps(candidate, indent=2) + "\n")
            (source / "vendor/worker-candidate.sha256").write_text(candidate["archive_sha256"] + "  aq-worker-source.tar.gz\n")
            self.record["prototype_only"] = True
        shutil.copy2(ROOT / "scripts/recovery-smoke-server.Dockerfile", source / "Dockerfile.recovery")
        inventory = {str(p.relative_to(source)): product.sha(p.read_bytes()) for p in sorted(source.rglob("*")) if p.is_file()}
        tree = product.sha(product.canonical(inventory))
        original_checker_root = product.checker.ROOT
        product.checker.ROOT = source
        candidate = product.checker.candidate()
        state_format = str(migration_state_format(source))
        server, worker = self.project + "-server:fixture", self.project + "-worker:fixture"
        self.images = [server, worker]
        self.record["recovery_build"] = {"source_inventory": inventory, "source_tree_sha256": tree,
                                          "feature": "recovery-test-support", "candidate": candidate}
        self.run(["docker", "build", "--target", "server", "-f", str(source / "Dockerfile.recovery"),
                  "--build-arg", "AQ_SOURCE_REVISION=recovery-" + tree[:24], "-t", server, str(source)],
                 name="build-fixture-server", timeout=1800)
        # Recovery drives run-execution, which only the CI-only synthetic-runtime target ships.
        self.run(["docker", "build", "--target", "synthetic-runtime", "-f", str(source / "Dockerfile.worker"),
                  "--build-arg", "AQ_SOURCE_REVISION=" + candidate["source_revision"],
                  "--build-arg", "AQ_WORKER_ARCHIVE_SHA256=" + candidate["archive_sha256"],
                  "--build-arg", "AQ_STATE_FORMAT=" + state_format,
                  "--build-arg", "AQ_VERSION=" + candidate["version"], "-t", worker, str(source)],
                 name="build-fixture-worker", timeout=1800)
        saved = {key: os.environ.get(key) for key in ["AQ_SMOKE_SERVER_IMAGE", "AQ_SMOKE_WORKER_IMAGE", "AQ_SMOKE_FIXTURE_IMAGE"]}
        os.environ.update(AQ_SMOKE_SERVER_IMAGE=server, AQ_SMOKE_WORKER_IMAGE=worker, AQ_SMOKE_FIXTURE_IMAGE=worker)
        try:
            super().setup()
        finally:
            product.checker.ROOT = original_checker_root
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
            self.images = [server, worker]
        self.server_image, self.worker_image = server, worker
        self.record["test_control"] = self.test_control()
        self.rules({})
        proxy_source = self.temp / "recovery-smoke-proxy.py"
        shutil.copy2(ROOT / "scripts/recovery-smoke-proxy.py", proxy_source)
        shutil.copy2(proxy_source, self.logs / "harness/recovery-smoke-proxy.actual.py")
        self.record["proxy_source_sha256"] = product.sha(proxy_source.read_bytes())
        self.proxy = self.project + "-proxy"
        self.run(["docker", "run", "-d", "--name", self.proxy,
                  "--label", "com.docker.compose.project=" + self.project,
                  "--network", self.project + "_edge", "--network-alias", "recovery-proxy",
                  "--user", f"{os.getuid()}:{os.getgid()}", "--read-only", "--cap-drop", "ALL",
                  "-v", str(proxy_source) + ":/app/proxy.py:ro", "-v", str(self.control) + ":/control",
                  "python:3.12-slim-bookworm", "python3", "-B", "/app/proxy.py", "--control", "/control"], name="start-proxy")
        self.poll("fault proxy readiness", lambda: any(x.get("event") == "proxy_ready" for x in self.proxy_events()))
        self.record["proxy_image"] = json.loads(self.run(["docker", "image", "inspect", "python:3.12-slim-bookworm"], name="proxy-image"))

    def test_control(self, **changes):
        return self.http("/api/execution/test-control", changes or None)

    def rules(self, value):
        temporary = self.control / "rules.tmp"
        temporary.write_text(json.dumps(value))
        temporary.replace(self.control / "rules.json")

    def proxy_events(self):
        path = self.control / "events.jsonl"
        if not path.exists():
            return []
        text = path.read_text()
        lines = text.splitlines()
        if text and not text.endswith("\n"):
            lines = lines[:-1]
        return [json.loads(line) for line in lines if line]

    def barrier(self, token):
        path = self.control / (token + ".ready")
        return self.poll("proxy barrier " + token, lambda: json.loads(path.read_text()) if path.exists() else None)

    def release(self, token):
        (self.control / (token + ".release")).touch()

    def start_case(self, name):
        self.case = name
        self.rules({})
        self.test_control(grant_ttl_ms=10000, bulk_blocked=False)
        case = {"id": name, "started_at": product.utc(), "snapshots": []}
        self.record["cases"].append(case)
        self.case_data = case
        config_volume, state_volume = self.project + "-" + name + "-config", self.project + "-" + name + "-state"
        self.config_volume = config_volume
        for volume in [config_volume, state_volume]:
            self.run(["docker", "volume", "create", "--label", "com.docker.compose.project=" + self.project, volume], name="create-" + volume.rsplit("-", 1)[-1])
        code = self.http("/api/enrollments", {}, expected=201)["code"]
        self.run(["docker", "run", "--rm", "-i", "--network", self.project + "_edge", "-v", config_volume + ":/config",
                  self.worker_image, "enroll", "--server", "http://recovery-proxy:8081", "--name", name], name="enroll", input=code + "\n")
        private = self.run(["docker", "run", "--rm", "--entrypoint", "cat", "-v", config_volume + ":/config",
                            self.worker_image, "/config/agent-q/worker.toml"], name="private-config", secret_output=True)
        config = tomllib.loads(private)
        self.credential = config["credential"]
        case["node_id"] = config["node_id"]
        self.clock_file = self.control / (name + "-clock-offset")
        self.clock_file.write_text("0")
        self.clock_file.chmod(0o644)
        self.keeper = self.project + "-" + name
        self.run(["docker", "run", "-d", "--init", "--name", self.keeper,
                  "--label", "com.docker.compose.project=" + self.project, "--network", self.project + "_edge",
                  "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
                  "-v", config_volume + ":/config", "-v", state_volume + ":/state",
                  "-v", str(self.control) + ":/fixture-control:ro", "--entrypoint", "sh", self.worker_image,
                  "-ec", "while :; do sleep 3600; done"], name="start-keeper")
        self.driver_number = 0

    def start_driver(self, checkpoint=None, *, hold=True, descendants=False):
        self.driver_number += 1
        prefix = f"/state/driver-{self.driver_number}"
        command = ["aq-worker", "run-execution", "--state-dir", "/state/runtime", "--fixture-agent", "/usr/local/bin/aq-fixture-agent",
                   "--idle-timeout-secs", "90", "--exit-after-result"]
        if checkpoint:
            command += ["--fault-checkpoint", checkpoint, "--fault-action", "pause"]
        # The recorded PID selects only the worker to crash. It is never used as
        # proof that descendant writers stopped; that requires guardian ECHILD.
        script = shlex.join(command) + f" >{prefix}.log 2>&1 & p=$!; ticks=$(awk '{{print $22}}' /proc/$p/stat); "
        script += f"printf '{{\"pid\":%s,\"start_ticks\":%s}}' \"$p\" \"$ticks\" >{prefix}.identity; wait \"$p\"; rc=$?; printf '%s' \"$rc\" >{prefix}.exit"
        args = ["docker", "exec", "-d", "-e", "AQ_GUARDIAN_CLOCK_OFFSET_FILE=/fixture-control/" + self.clock_file.name,
                "-e", "AQ_WORKER_CLOCK_OFFSET_FILE=/fixture-control/" + self.clock_file.name]
        if hold:
            args += ["-e", "AQ_FIXTURE_HOLD=1"]
        if descendants:
            args += ["-e", "AQ_FIXTURE_DESCENDANT=double-fork-setsid"]
        self.run([*args, self.keeper, "sh", "-c", script], name="start-driver-" + str(self.driver_number))
        self.driver_prefix = prefix
        self.poll("driver identity", lambda: self.read_json(prefix + ".identity", optional=True))

    def read(self, path, optional=False):
        if "token" in path or "worker.toml" in path:
            raise ValueError("private recovery file is not an evidence artifact")
        script = "cat " + shlex.quote(path)
        if optional:
            script = "if [ -f " + shlex.quote(path) + " ]; then " + script + "; fi"
        return self.run(["docker", "exec", self.keeper, "sh", "-c", script], name="read-" + Path(path).name, check=not optional)

    def read_json(self, path, optional=False):
        value = self.read(path, optional)
        return json.loads(value) if value else None

    def events(self):
        output = self.read(self.driver_prefix + ".log", optional=True)
        result = []
        for line in output.splitlines():
            try:
                result.append(json.loads(line))
            except ValueError:
                pass
        return result

    def registered(self):
        return self.poll("registered worker", lambda: next((e["identity"] for e in self.events() if e.get("event") == "registered"), None))

    def offer(self, identity, *, task_id=None, ttl_ms=60000):
        spec = {"project_id": str(uuid.uuid4()), "harness": "deterministic-agent-v1", "fixture_output": self.case,
                "resources": {"slots": 1, "cpu_millis": 100, "memory_bytes": 1048576, "disk_bytes": 1048576},
                "required_capabilities": ["deterministic-agent-v1", "linux-subreaper-guardian-v1"]}
        spec["resolved_spec_digest"] = product.sha(product.canonical(spec))
        now = self.test_control()["now_ms"]
        offer = {"offer_id": str(uuid.uuid4()), "identity": identity,
                 "attempt": {"attempt_id": str(uuid.uuid4()), "session_id": str(uuid.uuid4()), "generation": 1},
                 "spec": spec, "offer_expires_at_ms": now + 60000, "reservation_ttl_ms": ttl_ms}
        task_id = task_id or str(uuid.uuid4())
        self.http("/api/execution/fixtures", {"task_id": task_id, "offer": offer})
        self.case_data["task_id"] = task_id
        self.case_data["offer"] = offer
        return offer

    def inspect(self, offer):
        return self.http("/api/execution/attempts/" + offer["attempt"]["attempt_id"])

    def journal(self, label):
        value = self.read_json("/state/runtime/journal.json")
        self.case_data["snapshots"].append({"label": label, "journal": value})
        return value

    def cancel(self, offer):
        return self.http("/api/execution/attempts/" + offer["attempt"]["attempt_id"] + "/cancel",
                         {"operation_id": str(uuid.uuid4()), "reason": "recovery fixture cancellation"})

    def record_for(self, offer, label):
        return self.journal(label)["attempts"][offer["attempt"]["attempt_id"]]

    def running(self, offer):
        def inspect():
            state = self.read_json("/state/runtime/journal.json", optional=True)
            return state and state["attempts"].get(offer["attempt"]["attempt_id"], {}).get("phase") == "running"
        self.poll("running attempt", inspect)
        return self.record_for(offer, "running")

    def driver_exit(self, expected=None):
        value = self.poll("driver exit", lambda: self.read(self.driver_prefix + ".exit", optional=True))
        code = int(value)
        if expected is not None:
            assert code == expected, (code, self.read(self.driver_prefix + ".log"))
        return code

    def worker_signal(self, signal):
        identity = self.read_json(self.driver_prefix + ".identity")
        stat = self.read("/proc/" + str(identity["pid"]) + "/stat")
        assert int(stat.rsplit(") ", 1)[1].split()[19]) == identity["start_ticks"], "worker PID identity changed"
        self.run(["docker", "exec", self.keeper, "sh", "-c", "kill -" + signal + " " + str(identity["pid"])], name="worker-signal-" + signal)

    def touch(self, path):
        self.run(["docker", "exec", self.keeper, "touch", path], name="release-" + Path(path).name)

    def release_writers(self, launch_id):
        directory = "/state/runtime/launches/" + launch_id
        self.touch(directory + "/release-descendant")
        self.touch(directory + "/release")

    def stopped_result(self, offer):
        value = self.poll("durable result", lambda: (s if (s := self.inspect(offer)).get("disposition") else None))
        self.driver_exit(0)
        local = self.record_for(offer, "stopped-and-settled")
        proof = local.get("stopped_proof")
        assert value["stopped"] and value["report"]["stop_certainty"] == "confirmed_stopped"
        assert proof and proof["kernel_wait_echild"] and proof["phase"] == "stopped", "missing kernel guardian stop proof"
        assert proof["identity"]["launch_id"] == local["launch_id"]
        assert proof["identity"]["node_identity"] == offer["identity"]
        assert proof["identity"]["attempt"] == offer["attempt"]
        assert proof["identity"]["spec_digest"] == offer["spec"]["resolved_spec_digest"]
        self.case_data["result"] = value
        self.case_data["kernel_stop_proof"] = proof
        return value

    def advance_worker(self, delta):
        value = int(self.clock_file.read_text()) + delta
        temporary = self.clock_file.with_suffix(".new")
        temporary.write_text(str(value))
        temporary.chmod(0o644)
        temporary.replace(self.clock_file)
        self.case_data.setdefault("clock_advances", []).append({"worker_offset_ms": value})

    def same_task_refused(self, offer):
        replacement = json.loads(json.dumps(offer))
        replacement["offer_id"] = str(uuid.uuid4())
        replacement["attempt"] = {"attempt_id": str(uuid.uuid4()), "session_id": str(uuid.uuid4()), "generation": 1}
        replacement["offer_expires_at_ms"] = self.test_control()["now_ms"] + 60000
        error = self.http("/api/execution/fixtures", {"task_id": self.case_data["task_id"], "offer": replacement}, expected=409)
        assert error["code"] == "id_conflict"
        self.case_data["conflicting_replacement"] = error

    def case_transport(self, kind):
        self.start_case(kind)
        token = str(uuid.uuid4())
        route = "/v2/worker/accept" if kind in {"lost-acceptance", "late-grant"} else "/v2/worker/result"
        mode = "drop_after_commit" if kind == "lost-acceptance" else "hold_after_commit"
        if kind == "cancel-first":
            mode = "hold_before_forward"
        self.rules({route: {"mode": mode, "token": token, "once": kind == "lost-acceptance",
                            "barrier_before_drop": kind == "lost-acceptance"}})
        self.start_driver()
        identity = self.registered()
        offer = self.offer(identity, ttl_ms=1000 if kind == "late-grant" else 60000)
        if kind == "late-grant":
            self.barrier(token)
            self.advance_worker(2000)
            self.release(token)
            self.driver_exit(0)
            local = self.record_for(offer, "late-grant-refused")
            assert local["phase"] == "retired" and not local.get("launch_id") and not local["start_intent"]
            assert local["stop_certainty"] == "not_started"
            settled = self.inspect(offer)
            assert settled["stopped"] and settled["report"]["outcome"] == "expired" and not settled["report"]["claimed_success"]
            rejected = self.http("/v2/worker/accept", local["acceptance"], machine=True, credential=self.credential, expected=409)
            assert rejected["code"] in {"cancelled", "grant_expired"}
            self.case_data["terminal_reservation_replay"] = rejected
            self.case_data["result"] = settled
        else:
            first_grant_state = None
            if kind == "lost-acceptance":
                self.barrier(token)
                first_grant_state = self.inspect(offer)
                self.release(token)
            local = self.running(offer)
            if first_grant_state:
                replayed_state = self.inspect(offer)
                assert replayed_state["grant_expires_at_ms"] == first_grant_state["grant_expires_at_ms"]
                self.case_data["grant_absolute_deadline_ms"] = first_grant_state["grant_expires_at_ms"]
            self.release_writers(local["launch_id"])
            if kind != "lost-acceptance":
                self.barrier(token)
                before = self.inspect(offer)
                assert (before["disposition"] is not None) == (kind == "result-first")
                self.cancel(offer)
                self.release(token)
            value = self.stopped_result(offer)
            expected = "retained_cancelled" if kind == "cancel-first" else "accepted"
            assert value["disposition"]["disposition"] == expected
            replay = self.http("/v2/worker/result", value["report"], machine=True, credential=self.credential)
            assert replay == value["disposition"]
            if kind == "lost-acceptance":
                events = [e for e in self.proxy_events() if e.get("path") == route and e.get("operation_id") == local["acceptance"]["operation_id"]]
                assert any(e.get("event") == "connection_closed_after_upstream_commit" for e in events)
                assert len({e["body_sha256"] for e in events}) == 1
                assert len([e for e in events if e.get("event") == "upstream_response"]) >= 2
                responses = [e for e in events if e.get("event") == "upstream_response"]
                local_immutable = {key: value for key, value in local["grant"].items() if key != "valid_for_ms"}
                assert {e["immutable_grant_sha256"] for e in responses} == {product.sha(product.canonical(local_immutable))}
                ttls = [e["grant_remaining_ms"] for e in responses]
                assert all(later <= earlier for earlier, later in zip(ttls, ttls[1:]))
                self.case_data["transport_replay_events"] = events
        self.rules({})
        self.finish_case()

    def case_spawn_crash(self, checkpoint):
        self.start_case("crash-" + checkpoint if checkpoint else "worker-restart")
        self.start_driver(checkpoint, descendants=True)
        identity = self.registered()
        offer = self.offer(identity)
        if checkpoint == "report-durable":
            running = self.running(offer)
            self.release_writers(running["launch_id"])
        if checkpoint:
            marker = self.poll("durable crash checkpoint", lambda: self.read_json("/state/runtime/faults/" + checkpoint + ".json", optional=True))
        else:
            running = self.running(offer)
            marker = {"launch_id": running["launch_id"]}
            self.poll("live escaped descendant", lambda: self.read("/state/runtime/launches/" + running["launch_id"] + "/descendant-counter", optional=True))
        before = self.record_for(offer, "before-worker-kill")
        assert marker["launch_id"] == before["launch_id"]
        if checkpoint in {"start-sent-before-receipt", "start-receipt-durable"}:
            path = "/state/runtime/launches/" + before["launch_id"] + "/descendant-counter"
            first = int(self.poll("escaped descendant before crash", lambda: self.read(path, optional=True)))
            self.poll("escaped descendant writes before crash", lambda: int(self.read(path)) > first)
            self.case_data["escaped_descendant_observed"] = True
        self.worker_signal("KILL")
        self.driver_exit(137)
        self.start_driver(descendants=True)
        code = self.driver_exit()
        after = self.record_for(offer, "after-restart")
        assert after["launch_id"] == before["launch_id"], "crash caused blind new launch"
        if checkpoint in {None, "start-receipt-durable"}:
            assert after["stop_certainty"] == "confirmed_stopped", "surviving established guardian was not recovered"
        if after["stop_certainty"] == "confirmed_stopped":
            proof = after.get("stopped_proof")
            assert proof and proof["kernel_wait_echild"] and proof["phase"] == "stopped"
            assert proof["identity"]["attempt"] == offer["attempt"]
            if self.case_data.get("escaped_descendant_observed"):
                assert proof["reaped_count"] >= 2, "escaped descendant was not part of kernel stop proof"
            self.case_data["kernel_stop_proof"] = proof
        elif after["stop_certainty"] == "not_started":
            assert not after["start_intent"] and (after["launch_id"] is None or after["ownership_profile"] == "linux-subreaper-guardian-v1")
            assert after["phase"] == "retired", "proven nonstart was not reconciled"
            assert after["report"]["claimed_success"] is False
            self.case_data["nonstart_proof"] = "durable Start intent never crossed; guardian profile forbids writer fork before Start"
        else:
            assert after["stop_certainty"] == "stop_uncertain" and after["phase"] != "retired"
            assert after["offer"]["spec"]["resources"] == before["offer"]["spec"]["resources"]
            assert not any(e.get("event") == "registered" for e in self.events())
            self.same_task_refused(offer)
            self.case_data["explicit_uncertainty"] = True
        self.case_data["restart_exit_code"] = code
        self.case_data["daemon_after_restart"] = self.inspect(offer)
        self.finish_case()

    def case_guardian_loss(self):
        self.start_case("guardian-loss")
        self.start_driver(descendants=True)
        offer = self.offer(self.registered())
        record = self.running(offer)
        directory = "/state/runtime/launches/" + record["launch_id"]
        initial = int(self.poll("escaped writer counter", lambda: self.read(directory + "/descendant-counter", optional=True)))
        self.worker_signal("KILL")
        self.driver_exit(137)
        guardian = record["guardian"]["process"]
        stat = self.read("/proc/" + str(guardian["pid"]) + "/stat")
        assert int(stat.rsplit(") ", 1)[1].split()[19]) == guardian["start_ticks"]
        self.run(["docker", "exec", self.keeper, "sh", "-c", "kill -KILL " + str(guardian["pid"])], name="kill-guardian")
        self.poll("escaped writer survives guardian loss", lambda: int(self.read(directory + "/descendant-counter")) > initial)
        self.start_driver(descendants=True)
        assert self.driver_exit() != 0
        after = self.record_for(offer, "guardian-loss-recovery-hold")
        assert after["stop_certainty"] == "stop_uncertain" and after["phase"] != "retired"
        assert after["launch_id"] == record["launch_id"] and after.get("stopped_proof") is None
        hold = next((e for e in self.events() if e.get("event") == "recovery_hold"), None)
        assert hold and hold["protected_usage"]["slots"] >= 1 and hold["available"]["slots"] == 0
        self.same_task_refused(offer)
        self.case_data["recovery_hold"] = hold
        self.case_data["explicit_uncertainty"] = True
        self.case_data["operator_cleanup"] = "container PID namespace terminated; no central stopped receipt fabricated"
        self.finish_case()

    def case_old_binary(self):
        self.start_case("old-binary-current-journal-refusal")
        self.start_driver()
        offer = self.offer(self.registered())
        record = self.running(offer)
        self.release_writers(record["launch_id"])
        self.stopped_result(offer)
        journal = self.journal("current-journal-for-downgrade-test")
        expected_format = self.record["recovery_build"]["candidate"]["worker_journal_format"]
        assert journal["format_version"] == expected_format
        legacy = json.loads((ROOT / "scripts/recovery-smoke-legacy-worker.json").read_text())
        candidate = legacy["candidate"]
        assert product.sha((ROOT / "scripts" / legacy["dockerfile"]).read_bytes()) == legacy["dockerfile_sha256"]
        source = self.temp / "legacy-worker-source"
        (source / "vendor").mkdir(parents=True)
        raw = (ROOT / "scripts" / legacy["archive"]).read_bytes()
        assert product.sha(raw) == candidate["archive_sha256"]
        (source / "vendor/aq-worker-source.tar.gz").write_bytes(raw)
        (source / "vendor/worker-candidate.json").write_text(json.dumps(candidate, indent=2) + "\n")
        (source / "vendor/worker-candidate.sha256").write_text(candidate["archive_sha256"] + "  aq-worker-source.tar.gz\n")
        old_image = self.project + "-legacy-worker:fixture"
        self.images.append(old_image)
        self.run(["docker", "build", "-f", str(ROOT / "scripts" / legacy["dockerfile"]), "--target", "worker",
                  "--build-arg", "AQ_SOURCE_REVISION=" + candidate["source_revision"],
                  "--build-arg", "AQ_WORKER_ARCHIVE_SHA256=" + candidate["archive_sha256"],
                  "--build-arg", "AQ_STATE_FORMAT=2",
                  "--build-arg", "AQ_VERSION=" + candidate["version"], "-t", old_image, str(source)], name="build-old-binary", timeout=1800)
        copied_volume = self.project + "-journal-current-copy"
        self.run(["docker", "volume", "create", "--label", "com.docker.compose.project=" + self.project, copied_volume], name="create-journal-copy")
        body = product.canonical(journal).decode()
        self.run(["docker", "run", "--rm", "-i", "--entrypoint", "sh", "-v", copied_volume + ":/state", old_image,
                  "-c", "mkdir -p /state/runtime; chmod 700 /state/runtime; cat >/state/runtime/journal.json; chmod 600 /state/runtime/journal.json"],
                 name="copy-public-journal", input=body)
        boundary = len(self.proxy_events())
        output = self.run(["docker", "run", "--rm", "--network", self.project + "_edge", "-v", copied_volume + ":/state",
                           "-v", self.config_volume + ":/config", old_image, "run-execution", "--state-dir", "/state/runtime",
                           "--fixture-agent", "/usr/local/bin/aq-fixture-agent", "--idle-timeout-secs", "5", "--exit-after-result"],
                          name="old-binary-refusal", check=False, timeout=15)
        assert self.commands[-1]["exit_code"] != 0
        assert f"unsupported state format {expected_format}" in output or ("parse journal; refusing to advertise capacity" in output and "unknown field" in output)
        assert len(self.proxy_events()) == boundary, "old binary contacted daemon instead of refusing journal format"
        unchanged = self.run(["docker", "run", "--rm", "--entrypoint", "cat", "-v", copied_volume + ":/state", old_image,
                              "/state/runtime/journal.json"], name="unchanged-journal-copy")
        assert unchanged == body
        self.case_data["downgrade_refusal"] = output
        self.case_data["legacy_candidate"] = candidate
        self.case_data["legacy_image"] = json.loads(self.run(["docker", "image", "inspect", old_image], name="legacy-image"))
        self.finish_case()

    def case_partition(self):
        self.start_case("partition-expiry")
        self.start_driver(descendants=True)
        offer = self.offer(self.registered())
        local = self.running(offer)
        self.run(["docker", "network", "disconnect", self.project + "_edge", self.keeper], name="disconnect-worker-network")
        self.advance_worker(20000)
        self.test_control(advance_ms=20000)
        directory = "/state/runtime/launches/" + local["launch_id"]
        proof = self.poll("guardian expiry stop", lambda: (v if (v := self.read_json(directory + "/guardian-state.json", optional=True)) and v["phase"] == "stopped" else None))
        assert proof["kernel_wait_echild"] and proof["stop_reason"] == "expired"
        self.same_task_refused(offer)
        self.run(["docker", "network", "connect", self.project + "_edge", self.keeper], name="reconnect-worker-network")
        self.case_data["guardian_expiry_proof"] = proof
        value = self.poll("retained expired report", lambda: (v if (v := self.inspect(offer)).get("disposition") else None))
        assert value["report"]["outcome"] == "expired" and value["stopped"]
        assert value["disposition"]["disposition"] != "accepted"
        self.case_data["result"] = value
        self.finish_case()

    def case_daemon_restart(self):
        self.start_case("daemon-restart")
        self.test_control(grant_ttl_ms=30000)
        self.start_driver(descendants=True)
        offer = self.offer(self.registered())
        self.running(offer)
        self.rules({route: {"mode": "partition"} for route in ["/v2/worker/poll", "/v2/worker/renew"]})
        boundary = len(self.proxy_events())
        self.poll("partition barrier", lambda: any(e.get("event") == "partition_refused_before_forward" for e in self.proxy_events()[boundary:]))
        cancelled = self.cancel(offer)
        controls = self.test_control()
        self.compose("restart", "server", name="restart-daemon")
        self.address()
        self.poll("daemon health", lambda: self.http("/healthz"))
        self.test_control(set_offset_ms=controls["offset_ms"], grant_ttl_ms=controls["grant_ttl_ms"])
        restored = self.inspect(offer)
        assert restored == cancelled, "restart changed persisted authority/directive or extended grant"
        self.case_data["before_restart"] = cancelled
        self.case_data["after_restart"] = restored
        self.rules({})
        result = self.stopped_result(offer)
        assert result["disposition"]["disposition"] == "retained_cancelled"
        node = str(uuid.UUID(result["report"]["identity"]["node_id"]))
        report = str(uuid.UUID(result["report"]["report_id"]))
        def receipt_counts(label):
            sql = "SELECT (SELECT count(*) FROM execution_reports WHERE node_id='" + node + "' AND report_id='" + report + "'), (SELECT count(*) FROM execution_receipts WHERE node_id='" + node + "' AND operation_id='" + report + "')"
            value = self.compose("exec", "-T", "postgres", "psql", "-U", "agent_q", "-d", "agent_q", "-qAt", "-c", sql, name=label).strip()
            assert value == "1|1", "report history or receipt duplicated"
            return value
        counts = receipt_counts("receipt-count-before-restart")
        self.compose("restart", "server", name="restart-after-report-commit")
        self.address()
        self.poll("daemon after committed report", lambda: self.http("/healthz"))
        self.test_control(set_offset_ms=controls["offset_ms"], grant_ttl_ms=controls["grant_ttl_ms"])
        replay = self.http("/v2/worker/result", result["report"], machine=True, credential=self.credential)
        assert replay == result["disposition"] and self.inspect(offer) == result
        assert receipt_counts("receipt-count-after-restart-and-replay") == counts
        self.case_data["post_report_restart_replay"] = {"disposition": replay, "report_rows": 1, "receipt_rows": 1}
        self.finish_case()

    def case_old_result(self):
        self.start_case("old-result-after-replacement")
        self.start_driver()
        old = self.offer(self.registered())
        record = self.running(old)
        self.release_writers(record["launch_id"])
        settled = self.stopped_result(old)
        task = self.case_data["task_id"]
        self.start_driver()
        new = self.offer(self.registered(), task_id=task)
        current = self.running(new)
        self.worker_signal("STOP")
        try:
            before = self.inspect(new)
            replay = self.http("/v2/worker/result", settled["report"], machine=True, credential=self.credential)
            assert replay == settled["disposition"]
            changed = dict(settled["report"], output="changed old report")
            conflict = self.http("/v2/worker/result", changed, machine=True, credential=self.credential, expected=409)
            assert conflict["code"] == "id_conflict"
            after = self.inspect(new)
            assert after["offer"] == before["offer"] and after["report"] == before["report"] and after["disposition"] == before["disposition"]
        finally:
            self.worker_signal("CONT")
        assert current["launch_id"] != record["launch_id"]
        self.release_writers(current["launch_id"])
        self.stopped_result(new)
        self.case_data["old_result_disposition"] = replay
        self.case_data["new_attempt_id"] = new["attempt"]["attempt_id"]
        self.finish_case()

    def case_bulk(self):
        self.start_case("control-under-bulk")
        self.test_control(grant_ttl_ms=30000)
        self.start_driver(descendants=True)
        identity = self.registered()
        offer = self.offer(identity)
        self.running(offer)
        limits = self.test_control(bulk_blocked=True)
        count = limits["bulk_limit"]
        latencies = []
        pool = ThreadPoolExecutor(max_workers=count)
        # These are feature-gated synthetic bulk operations holding real general
        # DB connections and bulk HTTP permits, not simulated graph throughput.
        pending = [pool.submit(self.http, "/api/execution/test-bulk", {}) for _ in range(count)]
        try:
            saturated = self.poll("all bulk permits occupied", lambda: (v if (v := self.test_control())["bulk_active"] == count else None))
            busy = self.http("/api/execution/test-bulk", {}, expected=503)
            assert busy["error"] == "overloaded"
            boundary = len(self.proxy_events())
            self.advance_worker(11000)
            self.poll("real worker renew under bulk saturation", lambda: any(e.get("path") == "/v2/worker/renew" and e.get("status") == 200 for e in self.proxy_events()[boundary:]))
            for _ in range(40):
                started = time.monotonic()
                self.test_control()
                latencies.append((time.monotonic() - started) * 1000)
            started = time.monotonic()
            self.cancel(offer)
            acknowledgements = self.poll("cancellation recognition under load", lambda: [e for e in self.proxy_events()[boundary:] if e.get("path") == "/v2/worker/directive-ack" and e.get("status") == 200])
            recognition_ms = (time.monotonic() - started) * 1000
            result = self.stopped_result(offer)
            cancellation_ms = (time.monotonic() - started) * 1000
            assert acknowledgements, "worker never acknowledged cancellation under load"
            worker_controls = [e for e in self.proxy_events()[boundary:] if e.get("path") in {"/v2/worker/poll", "/v2/worker/renew"} and e.get("event") == "upstream_response"]
            assert worker_controls and all(e["status"] == 200 and e["upstream_latency_ms"] <= 1000 for e in worker_controls)
            assert result["disposition"]["disposition"] == "retained_cancelled"
            assert sorted(latencies)[int(len(latencies) * .95) - 1] <= 1000
            assert max(latencies) <= 2000 and cancellation_ms <= 2000 and 0 <= recognition_ms <= 1100
            self.case_data["control_load"] = {"bulk_profile": saturated, "control_inspection_count": len(latencies),
                                             "control_inspection_latency_ms": latencies, "worker_control_events": worker_controls,
                                             "cancellation_recognition_ms": recognition_ms, "cancellation_to_stopped_report_ms": cancellation_ms,
                                             "budgets_ms": {"control_p95": 1000, "control_max": 2000, "cancel_recognition": 1100, "cancel_to_stopped_report": 2000},
                                             "scope": "local2bulk-admission fixture; not500-agent or graph capacity acceptance"}
        finally:
            self.test_control(bulk_blocked=False)
            for future in pending:
                assert future.result(timeout=10)["released"]
            pool.shutdown(wait=True)
        self.finish_case()

    def case_lock_expiry(self):
        self.start_case("renew-expires-while-db-locked")
        self.test_control(grant_ttl_ms=30000)
        self.start_driver(descendants=True)
        offer = self.offer(self.registered())
        local = self.running(offer)
        self.worker_signal("STOP")
        before = self.inspect(offer)
        node = str(uuid.UUID(self.case_data["node_id"]))
        command = [*self.dc, "exec", "-T", "postgres", "psql", "-U", "agent_q", "-d", "agent_q", "-qAt", "-v", "ON_ERROR_STOP=1"]
        self.commands.append({"command": command, "purpose": "hold only disposable enrolled node row", "node_id": node})
        holder = subprocess.Popen(command, env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        pool = ThreadPoolExecutor(max_workers=1)
        try:
            holder.stdin.write("BEGIN; SELECT 'LOCK_HELD' FROM nodes WHERE id='" + node + "' FOR UPDATE;\n")
            holder.stdin.flush()
            assert select.select([holder.stdout], [], [], 10)[0], "node lock acquisition timed out"
            assert holder.stdout.readline().strip() == "LOCK_HELD"
            request = {"identity": offer["identity"], "operation_id": str(uuid.uuid4()), "attempt": offer["attempt"],
                       "grant_sequence": local["grant"]["grant_sequence"]}
            response = pool.submit(self.http, "/v2/worker/renew", request, machine=True, credential=self.credential, expected=409)
            def waiting():
                result = self.compose("exec", "-T", "postgres", "psql", "-U", "agent_q", "-d", "agent_q", "-qAt", "-c",
                    "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM nodes%FOR UPDATE%'",
                    name="observe-held-node-lock")
                return int(result.strip()) >= 1
            self.poll("renew waiting for actual node row lock", waiting)
            self.test_control(advance_ms=31000)
            self.advance_worker(31000)
            holder.stdin.write("COMMIT;\n\\q\n")
            holder.stdin.flush()
            holder.wait(timeout=5)
            error = response.result(timeout=5)
            assert error["code"] == "grant_expired"
            after = self.inspect(offer)
            assert after["grant_expires_at_ms"] == before["grant_expires_at_ms"] and after["grant"] == before["grant"]
            self.case_data["refused_stale_renewal"] = error
            self.case_data["deadline_before"] = before["grant_expires_at_ms"]
            self.case_data["deadline_after"] = after["grant_expires_at_ms"]
        finally:
            if holder.poll() is None:
                try:
                    holder.stdin.write("ROLLBACK;\n\\q\n")
                    holder.stdin.flush()
                    holder.wait(timeout=3)
                except (OSError, subprocess.SubprocessError):
                    holder.kill()
                    holder.wait()
            pool.shutdown(wait=True)
            self.worker_signal("CONT")
        result = self.stopped_result(offer)
        assert result["disposition"]["disposition"] == "retained_stale" and result["report"]["outcome"] == "expired"
        self.finish_case()

    def finish_case(self):
        self.case_data.update(status="passed", finished_at=product.utc())
        self.run(["docker", "stop", "-t", "1", self.keeper], name="stop-keeper", check=False)

    def cleanup(self):
        try:
            if self.case_data and self.case_data.get("status") == "failed" and hasattr(self, "keeper"):
                # Preserve the failed runtime's public evidence before removing
                # its disposable namespace. Never collect private control tokens.
                self.read("/state/runtime/journal.json", optional=True)
                for number in range(1, self.driver_number + 1):
                    self.read(f"/state/driver-{number}.log", optional=True)
                    self.read(f"/state/driver-{number}.exit", optional=True)
            if self.control.exists():
                for marker in self.control.glob("*.ready"):
                    marker.with_suffix(".release").touch()
                if (self.control / "events.jsonl").exists():
                    shutil.copy2(self.control / "events.jsonl", self.logs / "proxy-events.jsonl")
        except OSError as error:
            self.record["recovery_evidence_error"] = str(error)
        return super().cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--worker-prototype-revision", help="Explicit immutable development override; cannot close the gate")
    parser.add_argument("--worker-prototype-repository", type=Path, help="Local repository used only for explicit prototype git archive")
    args = parser.parse_args()
    checkpoints = ["grant-durable", "launch-intent-durable", "guardian-spawned", "guardian-identity-durable", "start-intent-durable",
                   "start-sent-before-receipt", "start-receipt-durable", "report-durable"]
    names = ["lost-acceptance", "late-grant", *["crash-" + point for point in checkpoints], "worker-restart",
             "guardian-loss", "partition-expiry", "cancel-first", "result-first", "old-result-after-replacement",
             "daemon-restart", "control-under-bulk", "old-binary-current-journal-refusal", "renew-expires-while-db-locked"]
    if args.list:
        print("\n".join(names))
        return 0
    selected = args.case or names
    if any(name not in names for name in selected):
        parser.error("unknown recovery case")
    if bool(args.worker_prototype_revision) != bool(args.worker_prototype_repository):
        parser.error("prototype override requires both repository and full commit")
    if args.worker_prototype_revision and not re.fullmatch(r"[0-9a-f]{40}", args.worker_prototype_revision):
        parser.error("prototype revision must be a full immutable commit")
    fixture = RecoveryFixture(args.worker_prototype_revision, args.worker_prototype_repository)
    result = 1
    try:
        fixture.setup()
        for name in selected:
            fixture.say("recovery case " + name)
            if name in {"lost-acceptance", "late-grant", "cancel-first", "result-first"}:
                fixture.case_transport(name)
            elif name.startswith("crash-"):
                fixture.case_spawn_crash(name.removeprefix("crash-"))
            elif name == "worker-restart":
                fixture.case_spawn_crash(None)
            else:
                {"guardian-loss": fixture.case_guardian_loss, "partition-expiry": fixture.case_partition,
                 "daemon-restart": fixture.case_daemon_restart, "old-result-after-replacement": fixture.case_old_result,
                 "control-under-bulk": fixture.case_bulk, "old-binary-current-journal-refusal": fixture.case_old_binary,
                 "renew-expires-while-db-locked": fixture.case_lock_expiry}[name]()
        fixture.record.update(outcome="passed", gate_status="matrix passed; requires review and exact-artifact acceptance before closing GATE-03",
                              complete_matrix=selected == names)
        result = 0
    except (OSError, ValueError, HTTPException, AssertionError, RuntimeError, subprocess.SubprocessError) as error:
        description = str(error) or type(error).__name__
        fixture.record.update(outcome="failed", error=description)
        if fixture.case_data is not None:
            fixture.case_data.update(status="failed", error=description)
        (fixture.logs / "failure.traceback.log").write_text(traceback.format_exc())
        print("recovery: " + description, file=sys.stderr)
    finally:
        if not fixture.cleanup():
            result = 1
    return result


if __name__ == "__main__":
    sys.exit(main())
