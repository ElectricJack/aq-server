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
        self.assertEqual(30, len(files))
        with self.assertRaisesRegex(ValueError, "empty real directory"):
            CHECK.extract(files, target)

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

    def test_manifest_digest_and_image_checksum_drift_refuse(self):
        self.pin["dashboard_digest"] = "sha256:" + "0" * 64
        self.save_pin()
        with self.assertRaisesRegex(ValueError, "manifest SHA256 mismatch"):
            CHECK.verify(self.root)
        self.pin = json.loads((CHECK.ROOT / "vendor/dashboard-candidate.json").read_bytes())
        self.save_pin()
        docker = self.root / "Dockerfile"
        docker.write_text(docker.read_text().replace(self.pin["archive_sha256"], "0" * 64))
        with self.assertRaisesRegex(ValueError, "Docker server stage"):
            CHECK.verify(self.root)


if __name__ == "__main__":
    unittest.main(verbosity=2)
