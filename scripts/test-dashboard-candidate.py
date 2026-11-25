#!/usr/bin/env python3
"""Exercise release verification failures without network or sibling repositories."""
import importlib.util
import io
import json
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("dashboard_candidate", Path(__file__).with_name("check-dashboard-candidate.py"))
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class DashboardCandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "vendor").mkdir()
        self.pin = json.loads((CHECK.ROOT / "vendor/dashboard-candidate.json").read_bytes())
        self.archive = self.pin["archive"]
        for name in ("dashboard-candidate.json", self.archive, self.archive + ".sha256", self.archive + ".release.json"):
            shutil.copyfile(CHECK.ROOT / "vendor" / name, self.root / "vendor" / name)
        shutil.copyfile(CHECK.ROOT / "Dockerfile", self.root / "Dockerfile")
        (self.root / "scripts").mkdir()
        shutil.copyfile(CHECK.ROOT / "scripts/patch-dashboard-terminal.py", self.root / "scripts/patch-dashboard-terminal.py")
        shutil.copytree(CHECK.ROOT / "vendor/dashboard-terminal-fix", self.root / "vendor/dashboard-terminal-fix")

    def save_pin(self):
        vendor = self.root / "vendor"
        (vendor / "dashboard-candidate.json").write_text(json.dumps(self.pin))
        record = {k: v for k, v in self.pin.items() if k not in ("component", "archive")}
        (vendor / (self.archive + ".release.json")).write_text(json.dumps(record))
        (vendor / (self.archive + ".sha256")).write_text(f"{self.pin['archive_sha256']}  {self.archive}\n")

    def repack(self, name, content, kind=tarfile.REGTYPE):
        archive_path = self.root / "vendor" / self.archive
        with tarfile.open(archive_path, "r:gz") as archive:
            entries = [(m.name, archive.extractfile(m).read()) for m in archive]
        with tarfile.open(archive_path, "w:gz") as archive:
            for path, data in entries + [(name, content)]:
                member = tarfile.TarInfo(path)
                member.size = len(data)
                if path == name:
                    member.type = kind
                    member.linkname = "/etc/passwd" if kind == tarfile.SYMTYPE else ""
                archive.addfile(member, io.BytesIO(data))
        self.pin["archive_sha256"] = CHECK.sha256(archive_path.read_bytes())
        self.save_pin()

    def test_published_bundle_extracts_exact_manifest_and_assets(self):
        pin, files = CHECK.verify(self.root)
        target = self.root / "extracted"
        target.mkdir()
        CHECK.extract(files, target)
        self.assertEqual(pin["dashboard_digest"], "sha256:" + CHECK.sha256((target / "dashboard-manifest.json").read_bytes()))
        extracted = {path.relative_to(target).as_posix(): path.read_bytes()
                     for path in target.rglob("*") if path.is_file()}
        self.assertEqual(files, extracted)
        with self.assertRaisesRegex(ValueError, "empty real directory"):
            CHECK.extract(files, target)

    def test_local_patch_has_truthful_provenance_and_fresh_cache_names(self):
        pin, files = CHECK.verify(self.root)
        manifest = json.loads(files["dashboard-manifest.json"])
        self.assertEqual(manifest["source"], dict(revision=self.pin["source_revision"], dirty=True, official=False))
        self.assertNotEqual(pin["dashboard_digest"], self.pin["dashboard_digest"])
        for name, data in files.items():
            if name != "dashboard-manifest.json":
                self.assertEqual(manifest["files"][name], dict(sha256=CHECK.sha256(data), bytes=len(data)))
        original = CHECK.read_archive(self.root / "vendor" / self.archive)
        old_names = {name for name in original if name.endswith((".js", ".css"))}
        self.assertTrue(old_names.isdisjoint(files))
        for name, data in files.items():
            if name.endswith((".js", ".css", ".html")):
                for old in old_names:
                    self.assertNotIn(old.removeprefix("assets/").encode(), data)
        # A second build must produce identical names and integrity metadata.
        self.assertEqual(CHECK.verify(self.root)[1], files)

    def test_patch_source_changes_invalidate_cache_namespace(self):
        before = CHECK.verify(self.root)[1]
        source = self.root / "vendor/dashboard-terminal-fix/manual-client.js"
        source.write_text(source.read_text() + "\n// changed local repair\n")
        after = CHECK.verify(self.root)[1]
        self.assertTrue({n for n in before if n.endswith(".js")}.isdisjoint(after))

    def test_patch_fails_closed_when_a_bundle_anchor_changes(self):
        spec = importlib.util.spec_from_file_location("terminal_patch", self.root / "scripts/patch-dashboard-terminal.py")
        patch = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(patch)
        files = CHECK.read_archive(self.root / "vendor" / self.archive)
        files[patch.ASSET] = files[patch.ASSET].replace(b"function de(e,t=", b"function changed(e,t=")
        with self.assertRaises(ValueError):
            patch.apply(files, self.root)

    def test_corrupted_archive_and_sidecar_refuse(self):
        archive = self.root / "vendor" / self.archive
        archive.write_bytes(archive.read_bytes() + b"corruption")
        with self.assertRaisesRegex(ValueError, "archive SHA256 mismatch"):
            CHECK.verify(self.root)
        self.pin["archive_sha256"] = CHECK.sha256(archive.read_bytes())
        (self.root / "vendor/dashboard-candidate.json").write_text(json.dumps(self.pin))
        with self.assertRaisesRegex(ValueError, "release record differs"):
            CHECK.verify(self.root)

    def test_unsafe_and_duplicate_archive_members_refuse(self):
        for name, kind in [("../escaped.js", tarfile.REGTYPE), ("assets/link.js", tarfile.SYMTYPE), ("index.html", tarfile.REGTYPE)]:
            with self.subTest(name=name):
                shutil.copyfile(CHECK.ROOT / "vendor" / self.archive, self.root / "vendor" / self.archive)
                self.repack(name, b"untrusted", kind)
                with self.assertRaisesRegex(ValueError, "unsafe archive|regular file|duplicate archive"):
                    CHECK.verify(self.root)

    def test_manifest_digest_and_image_wiring_drift_refuse(self):
        self.pin["dashboard_digest"] = "sha256:" + "0" * 64
        self.save_pin()
        with self.assertRaisesRegex(ValueError, "manifest SHA256 mismatch"):
            CHECK.verify(self.root)
        self.pin = json.loads((CHECK.ROOT / "vendor/dashboard-candidate.json").read_bytes())
        self.save_pin()
        docker = self.root / "Dockerfile"
        docker.write_text(docker.read_text().replace("COPY --from=dashboard-build /dashboard", "COPY --from=unverified /dashboard"))
        with self.assertRaisesRegex(ValueError, "Docker server stage"):
            CHECK.verify(self.root)


if __name__ == "__main__":
    unittest.main(verbosity=2)
