#!/usr/bin/env python3
"""Deploy the newest main-branch server image to the live aq-ms1 stack, with rollback.

  ms1-deploy.py [--config FILE] apply [--image REF] [--retry-failed]
  ms1-deploy.py [--config FILE] status
  ms1-deploy.py [--config FILE] rollback [--restore-backup]
  ms1-deploy.py [--config FILE] ack

`apply` follows `[deploy] image` (the `:main` tag server-image.yml moves) and is safe to run
from a timer: it does nothing when the server already runs that digest, or when the digest
already failed here. Otherwise it pulls the image, checks that only image fields change in
the rendered Compose config, turns the owner's availability off (no new launches; running
agents keep running on their workers), waits until no supervisor is mid-turn, takes a
pg_dump, recreates only the server, and waits for readiness at the image's revision and
state format and for every worker that was online to reconnect. Any failure rolls back
once: the server is stopped, the backup is restored if a migration already ran, and the
previous image is started again. A rollback that fails leaves `needs_operator` set and
availability off; `ack` clears it after a human has fixed the stack.

The image the stack runs is `AQ_SERVER_IMAGE` in `<state_dir>/release.env`, which this
script owns; the last Compose env file wins. `state.json` keeps the current and previous
release and the in-flight deploy, so a run cut short is finished or rolled back by the next.

Exit status: 0 done or nothing to do; 1 failed (rolled back, or needs an operator);
2 configuration error; 75 deferred (busy, or never quiet within quiet_timeout_secs).
"""
import argparse
import contextlib
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_CONFIG = Path.home() / ".config/agent-q/ms1-deploy.toml"
SERVICE = "server"
# compose.dev.yaml: the server keeps all of its state, playbooks included, in this database.
DATABASE = "agent_q"
DEFERRED = 75
REVISION = "org.opencontainers.image.revision"
STATE_FORMAT = "io.agent-q.state-format"
PG_ENV = 'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; exec "$@"'


class Failure(Exception):
    """A deploy step failed: the caller rolls back."""


class Deferred(Exception):
    """Not now: retry on the next run."""


@dataclass(frozen=True)
class Config:
    state_dir: Path
    image: str
    origin: str
    aq: str
    quiet_timeout_secs: int
    ready_timeout_secs: int
    reconnect_timeout_secs: int
    poll_secs: int
    backups_keep: int
    command_timeout_secs: int
    backup_timeout_secs: int
    project_name: str
    project_dir: Path
    files: tuple
    env_files: tuple

    @staticmethod
    def load(path):
        raw = tomllib.loads(Path(path).read_text())
        deploy, compose = raw["deploy"], raw["compose"]
        cfg = Config(
            state_dir=Path(deploy["state_dir"]), image=deploy["image"], origin=deploy["origin"],
            aq=deploy.get("aq", "aq"),
            quiet_timeout_secs=deploy.get("quiet_timeout_secs", 900),
            ready_timeout_secs=deploy.get("ready_timeout_secs", 600),
            reconnect_timeout_secs=deploy.get("reconnect_timeout_secs", 360),
            poll_secs=deploy.get("poll_secs", 5),
            backups_keep=deploy.get("backups_keep", 5),
            command_timeout_secs=deploy.get("command_timeout_secs", 120),
            backup_timeout_secs=deploy.get("backup_timeout_secs", 1800),
            project_name=compose["project_name"], project_dir=Path(compose["project_dir"]),
            files=tuple(compose["files"]), env_files=tuple(compose.get("env_files", ())),
        )
        if cfg.backups_keep < 1:
            raise ValueError("backups_keep must be at least 1: the newest backup is the rollback")
        if not cfg.state_dir.is_absolute() or not cfg.project_dir.is_absolute():
            raise ValueError("state_dir and compose.project_dir must be absolute")
        return cfg

    @property
    def container(self):
        return f"{self.project_name}-{SERVICE}-1"

    @property
    def postgres(self):
        return f"{self.project_name}-postgres-1"

    @property
    def release_env(self):
        return self.state_dir / "release.env"

    @property
    def backups(self):
        return self.state_dir / "backups"


def log(message):
    print(f"{datetime.now(timezone.utc):%Y-%m-%dT%H:%M:%SZ} {message}", flush=True)


def repository(ref):
    """`host/path` of an image reference, without tag or digest."""
    name = ref.split("@", 1)[0]
    head, _, tail = name.rpartition("/")
    return f"{head}/{tail.split(':', 1)[0]}" if head else tail.split(":", 1)[0]


def release(image_ref, info):
    """What state.json records about an image: the reference Compose starts and its labels."""
    labels = info["Config"]["Labels"] or {}
    revision, state_format = labels.get(REVISION, ""), labels.get(STATE_FORMAT, "")
    if not re.fullmatch(r"[0-9a-f]{40}", revision) or not state_format.isdigit():
        raise Failure(f"{image_ref} has no source revision or state format label")
    return {"image": image_ref, "id": info["Id"], "revision": revision, "state_format": int(state_format)}


def pinned(tag_ref, info):
    """The digest reference for `tag_ref`'s repository, so Compose starts exactly this image."""
    repo = repository(tag_ref)
    for digest in info.get("RepoDigests") or []:
        if repository(digest) == repo:
            return digest
    raise Failure(f"{tag_ref} has no registry digest for {repo}")


def changed_only_images(before, after, image):
    """Every difference between two rendered configs is an image field that becomes `image`."""
    a, b = before["services"], after["services"]
    if a.keys() != b.keys():
        return False
    for name in a:
        old, new = dict(a[name]), dict(b[name])
        if old.pop("image", None) != new.get("image") and new.get("image") != image:
            return False
        new.pop("image", None)
        if old != new:
            return False
    return {k: v for k, v in before.items() if k != "services"} == {k: v for k, v in after.items() if k != "services"}


def backups_to_prune(paths, keep, protected):
    """Oldest first beyond the newest `keep`; a backup a release still needs is never pruned."""
    ordered = sorted(paths, key=lambda p: p.name, reverse=True)
    return [p for p in ordered[keep:] if str(p) not in protected]


class Host:
    """Every side effect: docker, compose, the aq CLI and the server's HTTP API."""

    def __init__(self, cfg):
        self.cfg = cfg

    def run(self, argv, *, timeout=None, stdin=None, stdout=subprocess.PIPE, env=None, read=False):
        # Docker Desktop sometimes answers a read with empty output: reads are retried.
        for _ in range(3 if read else 1):
            done = subprocess.run(argv, stdin=stdin, stdout=stdout, stderr=subprocess.PIPE, text=True,
                                  timeout=timeout or self.cfg.command_timeout_secs, env=env)
            if done.returncode or not read or done.stdout.strip():
                break
            time.sleep(2)
        if done.returncode:
            raise Failure(f"{' '.join(map(str, argv[:4]))} exited {done.returncode}: {done.stderr.strip()[-400:]}")
        return done.stdout

    def compose(self, *args, image=None):
        env_file = self.cfg.release_env
        with tempfile.NamedTemporaryFile("w", dir=self.cfg.state_dir, suffix=".env") as candidate:
            if image is not None:
                candidate.write(f"AQ_SERVER_IMAGE={image}\n")
                candidate.flush()
                env_file = Path(candidate.name)
            argv = ["docker", "compose", "--project-name", self.cfg.project_name,
                    "--project-directory", str(self.cfg.project_dir)]
            for f in self.cfg.files:
                argv += ["-f", f]
            for e in (*self.cfg.env_files, env_file):
                argv += ["--env-file", str(e)]
            # The process environment beats every env file in Compose: keep ours out of it.
            env = {k: v for k, v in os.environ.items() if not k.startswith(("AQ_", "COMPOSE_"))}
            return self.run([*argv, *args], env=env, read=args[:1] == ("config",))

    def rendered(self, image):
        return json.loads(self.compose("config", "--format", "json", image=image))

    def pull(self, ref):
        self.run(["docker", "pull", "--quiet", ref], timeout=self.cfg.backup_timeout_secs)

    def image(self, ref):
        return json.loads(self.run(["docker", "image", "inspect", ref], read=True))[0]

    def running_image(self):
        """The image ID of the running server container, or None."""
        try:
            out = self.run(["docker", "inspect", "--format", "{{.State.Running}} {{.Image}}", self.cfg.container], read=True)
        except Failure:
            return None
        running, image = out.split()
        return image if running == "true" else None

    def readiness(self):
        out = self.run(["docker", "exec", self.cfg.container, "aq-server", "readiness"], read=True)
        deps = self.run(["docker", "exec", self.cfg.container, "aq-server", "dependency-readiness"], read=True)
        return {**json.loads(out), "dependencies": json.loads(deps).get("status")}

    def write_release(self, image):
        staged = self.cfg.release_env.with_suffix(".next")
        staged.write_text(f"AQ_SERVER_IMAGE={image}\n")
        staged.replace(self.cfg.release_env)

    def start(self, image):
        self.write_release(image)
        self.compose("up", "-d", "--no-deps", "--no-build", "--pull", "never", "--force-recreate", SERVICE)

    def stop(self):
        self.compose("stop", SERVICE)

    def psql(self, database, sql):
        return self.run(["docker", "exec", self.cfg.postgres, "sh", "-c", PG_ENV, "sh",
                         "psql", "-XAtqw", "-U", DATABASE, "-d", database, "-c", sql], read=True).strip()

    def database_format(self):
        return int(self.psql(DATABASE, "SELECT value FROM meta WHERE key = 'state_format_version'"))

    def backup(self, dest):
        with open(dest, "wb") as out:
            self.run(["docker", "exec", self.cfg.postgres, "sh", "-c", PG_ENV, "sh",
                      "pg_dump", "-w", "-Fc", "--create", "-U", DATABASE, "-d", DATABASE],
                     stdout=out, timeout=self.cfg.backup_timeout_secs)
        with open(dest, "rb") as dump:
            self.run(["docker", "exec", "-i", self.cfg.postgres, "pg_restore", "--list"],
                     stdin=dump, stdout=subprocess.DEVNULL, timeout=self.cfg.backup_timeout_secs)

    def restore(self, src):
        self.psql("postgres", f"DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)")
        with open(src, "rb") as dump:
            self.run(["docker", "exec", "-i", self.cfg.postgres, "sh", "-c", PG_ENV, "sh",
                      "pg_restore", "-w", "--create", "--exit-on-error", "-U", DATABASE, "-d", "postgres"],
                     stdin=dump, stdout=subprocess.DEVNULL, timeout=self.cfg.backup_timeout_secs)

    def remove_image(self, ref):
        with contextlib.suppress(Failure):
            self.run(["docker", "image", "rm", ref])

    def local_images(self, repo):
        out = self.run(["docker", "image", "ls", "--digests", "--format", "{{.Repository}}@{{.Digest}}", repo])
        return [line for line in out.split() if "@sha256:" in line]

    def aq(self, *args):
        return json.loads(self.run([self.cfg.aq, "--local", "--origin", self.cfg.origin, "--json", *args]))["data"]

    def swarm(self):
        out = self.run(["curl", "-fsS", "--max-time", "20", self.cfg.origin + "/api/v1/swarm"], read=True)
        return json.loads(out, strict=False)["data"]

    def supervisors(self):
        return self.aq("supervisor", "list")["supervisors"]

    def set_availability(self, available, generation):
        self.aq("swarm", "availability", "set", "--available", str(available).lower(),
                "--expected-generation", str(generation), "--operation-id", str(uuid.uuid4()),
                "--reason", "ms1-deploy server rollout")

    def sleep(self, seconds):
        time.sleep(seconds)

    def clock(self):
        return time.monotonic()


def eventually(host, timeout, check, what):
    deadline = host.clock() + timeout
    last = None
    while True:
        try:
            value = check()
            if value:
                return value
        except (Failure, ValueError, KeyError, json.JSONDecodeError) as error:
            last = error
        if host.clock() >= deadline:
            raise Failure(f"{what} not reached within {timeout}s" + (f" ({last})" if last else ""))
        host.sleep(host.cfg.poll_secs)


class Deployer:
    def __init__(self, cfg, host):
        self.cfg, self.host = cfg, host
        self.path = cfg.state_dir / "state.json"

    # ------------------------------------------------------------ state

    def load(self):
        if self.path.exists():
            return json.loads(self.path.read_text())
        # First run: adopt the running server, which release.env must already name.
        running = self.host.running_image()
        named = dict(line.split("=", 1) for line in self.cfg.release_env.read_text().split()) if self.cfg.release_env.exists() else {}
        image = named.get("AQ_SERVER_IMAGE")
        if not running or not image or self.host.image(image)["Id"] != running:
            raise ValueError(f"{self.cfg.release_env} must name the running server image before the first run")
        current = release(image, self.host.image(image))
        return {"current": current, "previous": None, "in_flight": None, "failed": {}, "needs_operator": None}

    def save(self, state):
        staged = self.path.with_suffix(".next")
        staged.write_text(json.dumps(state, indent=2) + "\n")
        staged.replace(self.path)

    # ------------------------------------------------------- availability

    def availability(self):
        value = self.host.swarm()["server"]["owner_availability"]
        return value["state"], int(value["generation"])

    def set_availability(self, on):
        for _ in range(3):
            current, generation = self.availability()
            if current == ("on" if on else "off"):
                return generation
            with contextlib.suppress(Failure):
                self.host.set_availability(on, generation)
        current, generation = self.availability()
        if current != ("on" if on else "off"):
            raise Failure(f"could not turn availability {'on' if on else 'off'}")
        return generation

    def pause(self):
        """Availability off if it is on; returns the generation we set, or None if untouched."""
        state, _ = self.availability()
        return self.set_availability(False) if state == "on" else None

    def resume(self, paused):
        """On again, unless someone changed availability after we paused it."""
        if paused is None:
            return
        with contextlib.suppress(Failure, ValueError, KeyError):
            state, generation = self.availability()
            if state == "off" and generation == paused:
                self.set_availability(True)
                log("availability restored")

    # ----------------------------------------------------------- checks

    def quiet(self):
        return not any(s["enabled"] and s["phase"] == "working" for s in self.host.supervisors())

    def online_workers(self):
        return {n["node"]["id"]: n["last_seen_at"] for m in self.host.swarm()["machines"]
                for n in m["nodes"] if n.get("health") == "online"}

    def ready(self, rel):
        if self.host.running_image() != rel["id"]:
            return False
        r = self.host.readiness()
        return (r.get("source_revision") == rel["revision"] and r.get("state_format") == rel["state_format"]
                and r.get("database") == "ok" and r.get("dependencies") == "ready")

    def verify(self, rel, workers):
        eventually(self.host, self.cfg.ready_timeout_secs, lambda: self.ready(rel), f"readiness of {rel['revision'][:12]}")
        log(f"server ready at {rel['revision'][:12]}, state format {rel['state_format']}")

        def back():
            now = self.online_workers()
            return {node for node, seen in workers.items() if node in now and now[node] > seen}
        try:
            eventually(self.host, self.cfg.reconnect_timeout_secs, lambda: back() == set(workers), "worker reconnection")
            log(f"all {len(workers)} workers reconnected")
        except Failure:
            # One machine asleep must not undo a deploy; none at all means the worker protocol broke.
            missing = set(workers) - back()
            if workers and missing == set(workers):
                raise Failure(f"none of the {len(workers)} online workers reconnected") from None
            log(f"warning: {len(missing)} workers did not reconnect: {sorted(missing)}")

    # --------------------------------------------------------- switching

    def roll_back(self, state, flight, reason):
        """Bring back flight['previous'] once; on failure leave needs_operator set."""
        previous = flight["previous"]
        log(f"rolling back to {previous['revision'][:12]}: {reason}")
        state["failed"][flight["target"]["image"]] = reason[:400]
        try:
            self.host.stop()
            found = self.host.database_format()
            if found != previous["state_format"]:
                if not flight["backup"]:
                    raise Failure(f"database is at state format {found} and no backup is recorded")
                log(f"database is at state format {found}: restoring {flight['backup']}")
                self.host.restore(flight["backup"])
            self.host.start(previous["image"])
            eventually(self.host, self.cfg.ready_timeout_secs, lambda: self.ready(previous), "readiness after rollback")
        except Failure as error:
            state["needs_operator"] = f"rollback to {previous['image']} failed: {error}"
            self.save(state)
            raise Failure(state["needs_operator"]) from None
        state["in_flight"] = None
        self.save(state)
        self.resume(flight.get("paused"))
        log("rolled back")

    def commit(self, state, flight):
        state["previous"], state["current"] = flight["previous"], {**flight["target"], "backup": flight["backup"]}
        state["in_flight"] = None
        self.save(state)
        self.resume(flight.get("paused"))
        self.prune(state)

    def recover(self, state):
        flight = state["in_flight"]
        log(f"finishing the interrupted deploy of {flight['target']['revision'][:12]}")
        try:
            if not self.ready(flight["target"]):
                raise Failure("the interrupted deploy is not ready")
        except Failure as error:
            self.roll_back(state, flight, str(error))
            return
        self.commit(state, flight)
        log("interrupted deploy committed")

    def prune(self, state):
        keep = {state[k]["backup"] for k in ("current", "previous") if state.get(k) and state[k].get("backup")}
        for path in backups_to_prune(list(self.cfg.backups.glob("*.dump")), self.cfg.backups_keep, keep):
            path.unlink()
        wanted = {state[k]["image"] for k in ("current", "previous") if state.get(k)}
        for ref in self.host.local_images(repository(self.cfg.image)):
            if ref not in wanted:
                self.host.remove_image(ref)

    # -------------------------------------------------------------- apply

    def apply(self, ref=None, retry_failed=False):
        state = self.load()
        if state["needs_operator"]:
            raise Failure(f"needs operator: {state['needs_operator']} (fix the stack, then run `ack`)")
        if state["in_flight"]:
            self.recover(state)
            return
        ref = ref or self.cfg.image
        self.host.pull(ref)
        info = self.host.image(ref)
        target = release(ref if "@sha256:" in ref else pinned(ref, info), info)
        current = state["current"]
        running = self.host.running_image()
        if running == target["id"]:
            log(f"server already runs {target['revision'][:12]}")
            return
        if running != current["id"]:
            raise Failure(f"the server runs {running}, not the recorded {current['image']}: it was changed by hand; "
                          f"put the running image in {self.cfg.release_env} and remove {self.path} to adopt it")
        if target["image"] in state["failed"] and not retry_failed:
            log(f"skipping {target['image']}: it failed here before ({state['failed'][target['image']]})")
            return
        refusal = None
        if target["state_format"] < current["state_format"]:
            refusal = f"refusing a downgrade from state format {current['state_format']} to {target['state_format']}"
        elif not changed_only_images(self.host.rendered(current["image"]), self.host.rendered(target["image"]), target["image"]):
            refusal = "the rendered Compose config changes more than the server image; fix the Compose files first"
        if refusal:
            state["failed"][target["image"]] = refusal
            self.save(state)
            raise Failure(refusal)
        log(f"deploying {target['revision'][:12]} ({target['image']}) over {current['revision'][:12]}")
        paused = self.pause()
        try:
            eventually(self.host, self.cfg.quiet_timeout_secs, self.quiet, "no supervisor mid-turn")
        except Failure:
            self.resume(paused)
            raise Deferred("supervisors stayed busy; trying again on the next run") from None
        workers = self.online_workers()
        self.cfg.backups.mkdir(mode=0o700, parents=True, exist_ok=True)
        backup = self.cfg.backups / f"{datetime.now(timezone.utc):%Y%m%dT%H%M%SZ}-{current['revision'][:12]}.dump"
        try:
            self.host.backup(backup)
        except Failure:
            backup.unlink(missing_ok=True)
            self.resume(paused)
            raise
        flight = {"target": target, "previous": current, "backup": str(backup), "paused": paused}
        state["in_flight"] = flight
        self.save(state)
        try:
            self.host.start(target["image"])
            self.verify(target, workers)
        except Failure as error:
            self.roll_back(state, flight, str(error))
            raise Failure(f"deploy of {target['revision'][:12]} failed and was rolled back: {error}") from None
        self.commit(state, flight)
        log(f"deployed {target['revision'][:12]}")

    def rollback(self, restore_backup):
        state = self.load()
        previous = state["previous"]
        if not previous:
            raise Failure("no previous release recorded")
        current = state["current"]
        if previous["state_format"] != current["state_format"] and not restore_backup:
            raise Failure(f"the previous release reads state format {previous['state_format']}, the database is at "
                          f"{current['state_format']}: pass --restore-backup to restore {current.get('backup')} "
                          "(every change since that backup is lost)")
        flight = {"target": current, "previous": previous, "backup": current.get("backup"), "paused": self.pause()}
        state["in_flight"] = flight
        self.save(state)
        self.roll_back(state, flight, "operator rollback")
        state["failed"].pop(current["image"], None)
        state["current"], state["previous"] = previous, None
        self.save(state)

    def ack(self):
        state = self.load()
        state["needs_operator"] = None
        self.save(state)

    def status(self):
        state = self.load()
        return {**state, "running_image": self.host.running_image()}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    sub = parser.add_subparsers(dest="command", required=True)
    apply = sub.add_parser("apply")
    apply.add_argument("--image", help="deploy this reference instead of [deploy] image")
    apply.add_argument("--retry-failed", action="store_true")
    sub.add_parser("status")
    rollback = sub.add_parser("rollback")
    rollback.add_argument("--restore-backup", action="store_true")
    sub.add_parser("ack")
    args = parser.parse_args(argv)
    try:
        cfg = Config.load(args.config)
        cfg.state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    except (OSError, KeyError, ValueError, tomllib.TOMLDecodeError) as error:
        print(f"ms1-deploy: configuration error: {error}", file=sys.stderr)
        return 2
    deployer = Deployer(cfg, Host(cfg))
    with open(cfg.state_dir / "lock", "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            log("another ms1-deploy holds the lock")
            return DEFERRED
        try:
            if args.command == "apply":
                deployer.apply(args.image, args.retry_failed)
            elif args.command == "status":
                print(json.dumps(deployer.status(), indent=2))
            elif args.command == "rollback":
                deployer.rollback(args.restore_backup)
            else:
                deployer.ack()
        except Deferred as error:
            log(str(error))
            return DEFERRED
        except Failure as error:
            log(f"failed: {error}")
            return 1
        except ValueError as error:
            print(f"ms1-deploy: {error}", file=sys.stderr)
            return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
