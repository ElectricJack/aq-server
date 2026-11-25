#!/usr/bin/env python3
"""Deployment state and artifact regressions without Docker or PostgreSQL."""
from contextlib import ExitStack, closing
from fcntl import LOCK_EX, LOCK_NB, flock
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tarfile
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch
from uuid import uuid4

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy/graph-deployment"


def load(name, path):
    loader = importlib.machinery.SourceFileLoader(name, str(path))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


plugin = load("graph_deploy", DEPLOY / "plugins/graph_deploy/bin/graph-deploy")
builder = load("build_image", DEPLOY / "build-image.py")


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.project = str(uuid4())
        self.revision = "a" * 40
        self.settings = {
            "environment": plugin.ENVIRONMENT, "instance": "a" * 12,
            "project_id": self.project,
        }
        (self.root / "environment.json").write_text(json.dumps(self.settings))
        (self.root / "provenance").mkdir()
        self.evidence = {
            "source": plugin.SOURCE, "revision": self.revision,
            "source_position": 1,
            **{key: "sha256:" + str(index) * 64 for index, key in enumerate((
                "image_digest", "image_id", "source_archive_digest", "image_archive_digest",
            ))},
        }
        self.write_evidence()
        self.db = plugin.open_state(self.root, timeout=0.1)
        self.addCleanup(self.db.close)
        with self.db:
            plugin.submit(self.db, self.root, {
                "revision": self.revision, "expected_generation": 0,
            })

    def write_evidence(self):
        path = self.root / "provenance" / f"{self.evidence['revision']}.json"
        path.write_text(json.dumps(self.evidence))

    def row(self):
        return plugin.latest(self.db)

    def running(self, **fields):
        with self.db:
            self.db.execute(
                "UPDATE deployment SET state='running', dispatch_at_ms=?", (plugin.now(),)
            )
            for field, value in fields.items():
                self.db.execute(f"UPDATE deployment SET {field}=?", (value,))
        return self.row()

    def container(self, status="running", health="starting"):
        row = self.row()
        return {
            "Id": "provider", "Image": row["image_id"],
            "Config": {"Image": row["image_id"], "Labels": {plugin.OP_LABEL: row["operation_id"]}},
            "State": {"Status": status, "Health": {"Status": health}},
        }

    def tick(self, docker):
        with patch.object(plugin, "Docker", return_value=docker):
            plugin.tick(self.root)

    def test_receipt_replay_conflict_and_project_binding(self):
        call = {
            "protocol": "aq.plugin/1", "project_id": self.project,
            "binding": {**self.settings, "directory": str(self.root)},
            "command": "ext.graph_deploy.request", "input": {"scheduled": False},
            "idempotency_key": str(uuid4()),
        }
        original = plugin.invoke(call)
        self.assertEqual(json.loads(original), plugin.response("ignored", None))
        self.assertEqual(plugin.invoke(call), original)
        call["input"] = {"scheduled": True}
        with self.assertRaisesRegex(plugin.Refused, "idempotency_conflict"):
            plugin.invoke(call)
        call["project_id"] = str(uuid4())
        with self.assertRaisesRegex(plugin.Refused, "deployment_disabled"):
            plugin.invoke(call)

    def test_health_deadline_and_healthy_observation(self):
        self.running(dispatch_at_ms=100)
        docker = Mock()
        docker.container.return_value = self.container()
        for elapsed, state in ((plugin.START_TIMEOUT_MS - 1, "running"),
                               (plugin.START_TIMEOUT_MS, "failed")):
            with self.subTest(elapsed=elapsed), patch.object(plugin, "now", return_value=100 + elapsed):
                plugin.reconcile(self.db, docker, self.row())
                self.assertEqual(self.row()["state"], state)
        docker.container.return_value = self.container(health="healthy")
        plugin.reconcile(self.db, docker, self.row())
        self.assertEqual(self.row()["state"], "healthy")
        self.assertEqual(self.row()["observed_digest"], self.evidence["image_digest"])
        docker.start.assert_not_called()

    def test_unhealthy_container_replaced_only_on_explicit_retry(self):
        self.running()
        docker = Mock()
        docker.container.return_value = self.container(status="exited")
        plugin.reconcile(self.db, docker, self.row())
        self.assertEqual(self.row()["diagnostic"], "graph_unhealthy")
        docker.run.assert_not_called()
        with self.db:
            plugin.submit(self.db, self.root, {"revision": self.revision, "expected_generation": 1})
        self.assertEqual(self.row()["retry_provider_id"], "provider")
        plugin.reconcile(self.db, docker, self.row())
        docker.verify_target.assert_called_once_with()
        docker.run.assert_called_once_with("rm", "-f", "provider")
        docker.start.assert_called_once()
        self.assertEqual(self.row()["state"], "running")

    def test_image_mismatch_and_missing_provider_are_terminal(self):
        self.running()
        docker = Mock()
        current = self.container()
        current["Config"]["Image"] = "other"
        docker.container.return_value = current
        plugin.reconcile(self.db, docker, self.row())
        self.assertEqual(self.row()["diagnostic"], "running_image_mismatch")
        docker.container.return_value = None
        plugin.reconcile(self.db, docker, self.row())
        self.assertEqual(self.row()["diagnostic"], "provider_instance_missing")
        docker.start.assert_not_called()

    def test_foreign_container_is_never_removed(self):
        docker = Mock()
        current = self.container()
        current["Config"]["Labels"][plugin.OP_LABEL] = str(uuid4())
        docker.container.return_value = current
        with self.assertRaisesRegex(plugin.Refused, "foreign_resource"):
            plugin.reconcile(self.db, docker, self.row())
        docker.run.assert_not_called()
        docker.start.assert_not_called()

    def test_staging_generation_change_fences_provider_mutations(self):
        def stage(row):
            # A separate writer must be able to submit while import is running.
            self.evidence.update(revision="b" * 40, source_position=2)
            self.write_evidence()
            with closing(plugin.open_state(self.root, timeout=0.1)) as db, db:
                plugin.submit(db, self.root, {
                    "revision": self.evidence["revision"], "expected_generation": 1,
                })

        docker = Mock()
        docker.stage_image.side_effect = stage
        self.tick(docker)
        self.assertEqual(self.row()["generation"], 2)
        self.assertEqual(self.row()["state"], "queued")
        docker.container.assert_not_called()
        docker.start.assert_not_called()

    def test_staging_failures_prevent_provider_mutations(self):
        for error, diagnostic in ((plugin.Refused("image_archive_mismatch"), "image_archive_mismatch"),
                                  (OSError(), "image_staging_failed"),
                                  (subprocess.TimeoutExpired("docker", 60), "image_staging_failed")):
            with self.subTest(error=error):
                self.db.execute("UPDATE deployment SET state='queued'")
                self.db.commit()
                docker = Mock()
                docker.stage_image.side_effect = error
                self.tick(docker)
                self.assertEqual(self.row()["state"], "failed")
                self.assertEqual(self.row()["diagnostic"], diagnostic)
                docker.container.assert_not_called()

    def test_provider_reconciliation_holds_write_lock(self):
        def container():
            with closing(plugin.open_state(self.root, timeout=0.1)) as db:
                with self.assertRaisesRegex(plugin.sqlite3.OperationalError, "locked"):
                    db.execute("BEGIN IMMEDIATE")
            return self.container(health="healthy")

        self.running()
        docker = Mock()
        docker.container.side_effect = container
        self.tick(docker)
        self.assertEqual(self.row()["state"], "healthy")
        docker.stage_image.assert_not_called()

    def test_archive_verified_before_loading_the_same_stream(self):
        members = {}

        def blob(value):
            data = json.dumps(value).encode()
            digest = "sha256:" + plugin.sha256(data).hexdigest()
            members[plugin.blob_path(digest)] = data
            return digest

        image_id = blob({"os": "linux", "architecture": "amd64", "config": {"Labels": {
            "org.opencontainers.image.source": plugin.SOURCE,
            "org.opencontainers.image.revision": self.revision,
        }}})
        image_digest = blob({"config": {"digest": image_id}})
        members["index.json"] = json.dumps({"manifests": [{"digest": image_digest}]}).encode()
        members["manifest.json"] = json.dumps([{
            "RepoTags": [], "Config": plugin.blob_path(image_id),
        }]).encode()
        archive = self.root / "provenance" / f"{self.revision}.tar"
        with tarfile.open(archive, "w") as layout:
            for name, data in members.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                layout.addfile(member, io.BytesIO(data))
        row = dict(self.row(), image_id=image_id, image_digest=image_digest,
                   image_archive_digest=builder.digest(archive))
        docker = plugin.Docker(self.root, self.settings)

        def imported(command, **options):
            self.assertEqual(command[-2:], ["image", "load"])
            self.assertEqual(options["timeout"], 60)
            self.assertEqual(options["stdin"].tell(), 0)
            self.assertEqual(options["stdin"].read(), archive.read_bytes())
            return Mock(returncode=0)

        with patch.object(plugin.subprocess, "run", side_effect=imported) as load_image:
            with patch.object(docker, "inspect", return_value={"Id": image_id}):
                docker.stage_image(row)
                load_image.assert_called_once()
                load_image.reset_mock()
                row["revision"] = "b" * 40
                archive.rename(archive.with_name(f"{row['revision']}.tar"))
                with self.assertRaisesRegex(plugin.Refused, "image_provenance_mismatch"):
                    docker.stage_image(row)
                load_image.assert_not_called()
                row["image_archive_digest"] = "sha256:" + "0" * 64
                with self.assertRaisesRegex(plugin.Refused, "image_archive_mismatch"):
                    docker.stage_image(row)
                load_image.assert_not_called()

    def test_merge_claim_waits_for_build_and_detects_changed_evidence(self):
        self.running()
        self.db.execute("UPDATE deployment SET state='healthy'")
        evidence = {"operation_id": str(uuid4()), "merge_sha": "b" * 40,
                    "pull_request": 1, "observed_at_ms": 1}
        with patch.object(plugin, "merge_evidence", return_value=[evidence]):
            result = plugin.claim_merge(self.db, self.root, {"merge_source": "configured"})
        self.assertEqual(result, plugin.response("idle", {"reason": "waiting_for_build"}))
        self.evidence.update(revision=evidence["merge_sha"], source_position=2)
        self.write_evidence()
        with patch.object(plugin, "merge_evidence", return_value=[evidence]):
            result = plugin.claim_merge(self.db, self.root, {"merge_source": "configured"})
        self.assertEqual(result["output"]["generation"], 2)
        self.db.execute("UPDATE deployment SET state='healthy'")
        evidence["pull_request"] = 2
        with patch.object(plugin, "merge_evidence", return_value=[evidence]):
            with self.assertRaisesRegex(plugin.Refused, "merge_evidence_changed"):
                plugin.claim_merge(self.db, self.root, {"merge_source": "configured"})

    def test_builder_publishes_archive_before_receipt_under_lock(self):
        self.db.execute("INSERT INTO merge_claim VALUES (?, ?, 1, 1, 'pending', NULL)",
                        (str(uuid4()), self.revision))
        self.db.commit()
        replacements = []
        replace = builder.os.replace

        def build(directory, repo, revision, instance, cleanup):
            with (self.root / "builder.lock").open("a") as lock:
                with self.assertRaises(BlockingIOError):
                    flock(lock, LOCK_EX | LOCK_NB)
            for suffix in ("tar", "json"):
                (directory / "provenance" / f"{revision}.{suffix}").write_text(suffix)

        def publish(source, destination):
            replacements.append(destination.suffix)
            if destination.suffix == ".json":
                self.assertTrue(destination.with_suffix(".tar").is_file())
            replace(source, destination)

        (self.root / "provenance" / f"{self.revision}.json").unlink()
        with patch.object(builder, "build", side_effect=build) as mocked_build:
            with patch.object(builder.os, "replace", side_effect=publish):
                builder.build_next(self.root, Path("repository"))
                builder.build_next(self.root, Path("repository"))
        self.assertEqual(replacements, [".tar", ".json"])
        mocked_build.assert_called_once()

    def test_build_keeps_revision_profile_deadlines_and_receipt(self):
        image_id, image_digest = ("sha256:" + c * 64 for c in "ab")
        calls = []

        def archive(command, **options):
            self.assertEqual(command[-3:], ["archive", "--format=tar", self.revision])
            self.assertEqual(options["timeout"], 30)
            options["stdout"].write(b"source archive")

        def docker(directory, *args, **options):
            calls.append((args, options.get("timeout")))
            if args[0] == "buildx":
                self.assertIn("CARGO_PROFILE=ci-image", args)
                self.assertIn("linux/amd64", args)
                self.assertIn(f"org.opencontainers.image.revision={self.revision}", args)
                self.assertEqual(options["stdin"].read(), b"source archive")
                (directory / f"{self.revision}.id").write_text(image_id)
            elif args[:2] == ("image", "save"):
                data = json.dumps({"manifests": [{"digest": image_digest}]}).encode()
                with tarfile.open(args[3], "w") as layout:
                    member = tarfile.TarInfo("index.json")
                    member.size = len(data)
                    layout.addfile(member, io.BytesIO(data))

        with patch.object(builder, "history", return_value=["b" * 40, self.revision]):
            with patch.object(builder.subprocess, "run", side_effect=archive):
                with patch.object(builder, "docker", side_effect=docker), ExitStack() as cleanup:
                    builder.build(self.root, Path("repository"), self.revision, "a" * 12, cleanup)
        evidence = json.loads((self.root / "provenance" / f"{self.revision}.json").read_text())
        self.assertEqual(evidence["source_position"], 1)
        self.assertEqual(evidence["image_id"], image_id)
        self.assertEqual(evidence["image_digest"], image_digest)
        self.assertEqual(evidence["source_archive_digest"], builder.digest(self.root / f"{self.revision}.source.tar"))
        self.assertEqual(evidence["image_archive_digest"], builder.digest(self.root / "provenance" / f"{self.revision}.tar"))
        self.assertEqual([timeout for _, timeout in calls], [900, 90, None])
        self.assertEqual(calls[-1][0][:2], ("image", "rm"))


if __name__ == "__main__":
    unittest.main()
