"""ms1-deploy.py's deploy, rollback and recovery decisions, against a fake host (no Docker, no clock)."""
import importlib.util
import json
from pathlib import Path

import pytest

spec = importlib.util.spec_from_file_location("ms1_deploy", Path(__file__).with_name("ms1-deploy.py"))
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)

REPO = "ghcr.io/aq-platform/aq-server"
OLD, NEW = "a" * 40, "b" * 40
OLD_REF, NEW_REF = f"{REPO}@sha256:{'1' * 64}", f"{REPO}@sha256:{'2' * 64}"


def info(ident, revision, state_format, digest=None):
    return {"Id": ident, "RepoDigests": [digest] if digest else [],
            "Config": {"Labels": {deploy.REVISION: revision, deploy.STATE_FORMAT: str(state_format)}}}


class FakeHost:
    def __init__(self, cfg, new_format=185):
        self.cfg = cfg
        self.images = {OLD_REF: info("sha256:old", OLD, 184, OLD_REF),
                       f"{REPO}:main": info("sha256:new", NEW, new_format, NEW_REF),
                       NEW_REF: info("sha256:new", NEW, new_format, NEW_REF)}
        self.running = "sha256:old"
        self.db_format = 184
        self.broken = set()           # image ids whose server never gets ready
        self.migrates = True          # a started image migrates the database to its format
        self.availability = ["on", 7]
        self.busy_polls = 0           # supervisor polls that report a working supervisor
        self.reconnecting = {"n1", "n2"}
        self.seen = {"n1": "2026-11-26T10:00:00.000Z", "n2": "2026-11-26T10:00:00.000Z"}
        self.now = 0.0
        self.calls = []
        self.fail_start_of = set()

    def pull(self, ref):
        self.calls.append(("pull", ref))

    def image(self, ref):
        return self.images[ref]

    def running_image(self):
        return self.running

    def rendered(self, image):
        return {"name": "aq-ms1", "services": {"server": {"image": image, "user": "10001"},
                                                "dev-state-init": {"image": image}, "postgres": {"image": "pg"}}}

    def start(self, image):
        self.calls.append(("start", image))
        ident = self.images[image]["Id"]
        if ident in self.fail_start_of:
            raise deploy.Failure("compose up failed")
        self.running = ident
        if self.migrates:
            self.db_format = int(self.images[image]["Config"]["Labels"][deploy.STATE_FORMAT])
        for node in self.reconnecting:
            self.seen[node] = "2026-11-26T10:05:00.000Z"

    def write_release(self, image):
        self.calls.append(("release", image))

    def stop(self):
        self.calls.append(("stop",))
        self.running = None

    def readiness(self):
        if self.running in self.broken:
            raise deploy.Failure("not ready")
        labels = next(i for i in self.images.values() if i["Id"] == self.running)["Config"]["Labels"]
        return {"source_revision": labels[deploy.REVISION], "state_format": int(labels[deploy.STATE_FORMAT]),
                "database": "ok", "dependencies": "ready"}

    def database_format(self):
        return self.db_format

    def backup(self, dest):
        self.calls.append(("backup",))
        Path(dest).write_bytes(b"dump")

    def restore(self, src):
        self.calls.append(("restore", src))
        self.db_format = 184

    def local_images(self, repo):
        return [OLD_REF, NEW_REF, f"{REPO}@sha256:{'0' * 64}"]

    def remove_image(self, ref):
        self.calls.append(("rm", ref))

    def swarm(self):
        nodes = [{"node": {"id": n}, "health": "online", "last_seen_at": t} for n, t in self.seen.items()]
        return {"server": {"owner_availability": {"state": self.availability[0], "generation": str(self.availability[1])}},
                "machines": [{"nodes": nodes}]}

    def supervisors(self):
        if self.busy_polls:
            self.busy_polls -= 1
            return [{"enabled": True, "phase": "working"}]
        return [{"enabled": True, "phase": "idle"}, {"enabled": False, "phase": "working"}]

    def set_availability(self, available, generation):
        assert generation == self.availability[1]
        self.availability = ["on" if available else "off", generation + 1]
        self.calls.append(("availability", available))

    def sleep(self, seconds):
        self.now += seconds

    def clock(self):
        return self.now


@pytest.fixture
def cfg(tmp_path):
    path = tmp_path / "ms1-deploy.toml"
    path.write_text(f"""
[deploy]
state_dir = "{tmp_path / 'state'}"
image = "{REPO}:main"
origin = "http://127.0.0.1:28080"
quiet_timeout_secs = 60
ready_timeout_secs = 30
reconnect_timeout_secs = 30
backups_keep = 2
[compose]
project_name = "aq-ms1"
project_dir = "/srv/aq"
files = ["/srv/aq/compose.yaml"]
""")
    config = deploy.Config.load(path)
    config.state_dir.mkdir()
    config.release_env.write_text(f"AQ_SERVER_IMAGE={OLD_REF}\n")
    return config


def deployer(cfg, **kw):
    host = FakeHost(cfg, **kw)
    return deploy.Deployer(cfg, host), host


def state(cfg):
    return json.loads((cfg.state_dir / "state.json").read_text())


def test_deploys_the_main_digest_and_restores_availability(cfg):
    d, host = deployer(cfg)
    d.apply()
    s = state(cfg)
    assert s["current"]["image"] == NEW_REF and s["current"]["revision"] == NEW
    assert s["previous"]["image"] == OLD_REF and s["in_flight"] is None
    assert ("start", NEW_REF) in host.calls and ("backup",) in host.calls
    assert host.availability[0] == "on"
    assert [c for c in host.calls if c[0] == "availability"] == [("availability", False), ("availability", True)]
    assert host.calls[-1] == ("rm", f"{REPO}@sha256:{'0' * 64}")  # only images no release uses


def test_second_run_is_a_no_op(cfg):
    d, host = deployer(cfg)
    d.apply()
    host.calls.clear()
    d.apply()
    assert [c[0] for c in host.calls] == ["pull"]


def test_leaves_availability_off_when_the_owner_turned_it_off(cfg):
    d, host = deployer(cfg)
    host.availability = ["off", 3]
    d.apply()
    assert host.availability == ["off", 3]


def test_never_ready_without_migration_swaps_back_without_restore(cfg):
    d, host = deployer(cfg)
    host.broken.add("sha256:new")
    host.migrates = False
    with pytest.raises(deploy.Failure, match="rolled back"):
        d.apply()
    assert not any(c[0] == "restore" for c in host.calls)
    assert host.running == "sha256:old" and host.calls[-2:][0] == ("start", OLD_REF)
    s = state(cfg)
    assert s["current"]["image"] == OLD_REF and NEW_REF in s["failed"] and s["in_flight"] is None
    assert host.availability[0] == "on"
    host.calls.clear()
    d.apply()  # the failed digest is not retried by the timer
    assert [c[0] for c in host.calls] == ["pull"]


def test_failure_after_migration_restores_the_backup(cfg):
    d, host = deployer(cfg)
    host.broken.add("sha256:new")
    with pytest.raises(deploy.Failure):
        d.apply()
    restore = [c for c in host.calls if c[0] == "restore"]
    assert len(restore) == 1 and restore[0][1].endswith(f"-{OLD[:12]}.dump")
    assert host.calls.index(("stop",)) < host.calls.index(restore[0]) < host.calls.index(("start", OLD_REF))
    assert host.db_format == 184 and host.running == "sha256:old"


def test_failed_rollback_needs_an_operator(cfg):
    d, host = deployer(cfg)
    host.broken.update({"sha256:new", "sha256:old"})
    with pytest.raises(deploy.Failure, match="rollback"):
        d.apply()
    assert state(cfg)["needs_operator"]
    assert host.availability[0] == "off"
    with pytest.raises(deploy.Failure, match="needs operator"):
        d.apply()
    d.ack()
    assert state(cfg)["needs_operator"] is None


def test_busy_supervisors_defer_without_touching_the_server(cfg):
    d, host = deployer(cfg)
    host.busy_polls = 10 ** 6
    with pytest.raises(deploy.Deferred):
        d.apply()
    assert not any(c[0] in ("start", "backup", "stop") for c in host.calls)
    assert host.availability[0] == "on"


def test_waits_for_supervisors_to_finish(cfg):
    d, host = deployer(cfg)
    host.busy_polls = 3
    d.apply()
    assert state(cfg)["current"]["image"] == NEW_REF


def test_a_sleeping_worker_does_not_undo_a_deploy(cfg):
    d, host = deployer(cfg)
    host.reconnecting = {"n1"}
    d.apply()
    assert state(cfg)["current"]["image"] == NEW_REF


def test_no_worker_reconnecting_rolls_back(cfg):
    d, host = deployer(cfg)
    host.reconnecting = set()
    with pytest.raises(deploy.Failure, match="none of the 2 online workers reconnected"):
        d.apply()
    assert host.running == "sha256:old" and state(cfg)["current"]["image"] == OLD_REF


def test_refuses_a_state_format_downgrade(cfg):
    d, host = deployer(cfg, new_format=183)
    with pytest.raises(deploy.Failure, match="downgrade"):
        d.apply()
    assert not any(c[0] in ("start", "backup") for c in host.calls)
    assert NEW_REF in state(cfg)["failed"]


def test_refuses_a_server_changed_by_hand(cfg):
    d, host = deployer(cfg)
    d.apply()
    host.running = "sha256:other"
    host.images[f"{REPO}:main"] = info("sha256:newer", "c" * 40, 186, f"{REPO}@sha256:{'3' * 64}")
    with pytest.raises(deploy.Failure, match="by hand"):
        d.apply()


def test_an_interrupted_deploy_is_committed_when_ready_or_rolled_back(cfg):
    d, host = deployer(cfg)
    s = d.load()
    target = deploy.release(NEW_REF, host.images[NEW_REF])
    s["in_flight"], s["paused"] = {"target": target, "previous": s["current"], "backup": None}, 7
    d.save(s)
    host.running, host.availability, host.db_format = "sha256:new", ["off", 7], 185
    d.apply()
    s = state(cfg)
    assert s["current"]["image"] == NEW_REF and s["in_flight"] is None and host.availability[0] == "on"

    s["in_flight"] = {"target": deploy.release(OLD_REF, host.images[OLD_REF]) | {"image": "x", "id": "sha256:gone"},
                      "previous": s["current"], "backup": None}
    d.save(s)
    d.apply()  # the target is not running: roll back to the recorded previous release
    s = state(cfg)
    assert s["in_flight"] is None and "x" in s["failed"] and host.running == "sha256:new"


def test_operator_rollback_needs_restore_across_state_formats(cfg):
    d, host = deployer(cfg)
    d.apply()
    with pytest.raises(deploy.Failure, match="--restore-backup"):
        d.rollback(False)
    d.rollback(True)
    s = state(cfg)
    assert s["current"]["image"] == OLD_REF and s["previous"] is None
    assert host.running == "sha256:old" and any(c[0] == "restore" for c in host.calls)


def test_compose_change_check():
    before = {"name": "p", "services": {"server": {"image": "a", "x": 1}, "init": {"image": "a"}, "pg": {"image": "pg"}}}
    after = json.loads(json.dumps(before))
    after["services"]["server"]["image"] = after["services"]["init"]["image"] = "b"
    assert deploy.changed_only_images(before, after, "b")
    after["services"]["pg"]["image"] = "b2"
    assert not deploy.changed_only_images(before, after, "b")
    after["services"]["pg"]["image"] = "pg"
    after["services"]["server"]["x"] = 2
    assert not deploy.changed_only_images(before, after, "b")


def test_backup_pruning_keeps_newest_and_protected():
    paths = [Path(f"/b/2026112{i}T000000Z-x.dump") for i in range(5)]
    pruned = deploy.backups_to_prune(paths, 2, {str(paths[0])})
    assert pruned == [paths[2], paths[1]]


def test_repository_and_pinning():
    assert deploy.repository("ghcr.io/o/aq-server:main") == "ghcr.io/o/aq-server"
    assert deploy.repository("localhost:5000/aq@sha256:" + "1" * 64) == "localhost:5000/aq"
    assert deploy.pinned(f"{REPO}:main", info("i", NEW, 1, NEW_REF)) == NEW_REF
    with pytest.raises(deploy.Failure):
        deploy.pinned(f"{REPO}:main", info("i", NEW, 1, "other/repo@sha256:" + "1" * 64))
    with pytest.raises(deploy.Failure):
        deploy.release("x", {"Id": "i", "Config": {"Labels": {}}})


def test_a_run_that_died_with_availability_off_is_followed_by_one_that_restores_it(cfg):
    d, host = deployer(cfg)
    s = d.load()
    s["paused"] = 8
    d.save(s)
    host.availability = ["off", 8]
    host.running = "sha256:new"  # nothing to deploy: the restore still happens first
    s["current"] = deploy.release(NEW_REF, host.images[NEW_REF])
    d.save(s)
    d.apply()
    assert host.availability[0] == "on" and state(cfg)["paused"] is None
