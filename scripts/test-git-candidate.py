#!/usr/bin/env python3
"""Exercise retained release verification, including tampering refusals."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('git_candidate', ROOT / 'scripts/check-git-candidate.py')
CHECKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'vendor').mkdir()
        (self.root / 'scripts').mkdir()
        self.manifest = json.loads((ROOT / 'vendor/git-candidate.json').read_text())
        for name in ('Dockerfile', 'Dockerfile.worker', 'scripts/Dockerfile.aq-git'):
            shutil.copyfile(ROOT / name, self.root / name)
        paths = {self.manifest['archive'], 'aq-git-source.bundle', 'git-candidate.sha256'}
        paths.update(self.manifest[k] for k in ('binary', 'build_record', 'inventory') if k in self.manifest)
        for path in paths:
            target = self.root / 'vendor' / path
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / 'vendor' / path, target)
        directory = Path(self.manifest['git']['archive']).parent
        shutil.copytree(ROOT / 'vendor' / directory, self.root / 'vendor' / directory, dirs_exist_ok=True)
        self.save_manifest()
        self.root_patch = patch.object(CHECKER, 'ROOT', self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        self.env_patch = patch.dict(os.environ, {'AQ_GIT_BIN': '', 'AQ_GIT_SOURCE_DIR': ''})
        self.env_patch.start()
        self.addCleanup(self.env_patch.stop)

    def save_manifest(self):
        (self.root / 'vendor/git-candidate.json').write_text(json.dumps(self.manifest))

    def test_retained_release_is_accepted(self):
        self.assertEqual(CHECKER.candidate(official=True)['version'], '0.2.0')

    def test_changed_binary_refused(self):
        with (self.root / 'vendor' / self.manifest['binary']).open('ab') as stream:
            stream.write(b'changed')
        with self.assertRaisesRegex(ValueError, 'binary digest mismatch'):
            CHECKER.candidate(official=True)

    def test_changed_build_record_refused(self):
        record = self.root / 'vendor' / self.manifest['build_record']
        data = json.loads(record.read_text())
        data['source_revision'] = '0' * 40
        record.write_text(json.dumps(data))
        with self.assertRaisesRegex(ValueError, 'build record'):
            CHECKER.candidate(official=True)

    def test_outside_binary_path_refused(self):
        self.manifest['binary'] = '../outside'
        (self.root / 'outside').write_bytes(b'release binary')
        self.manifest['binary_sha256'] = hashlib.sha256(b'release binary').hexdigest()
        self.save_manifest()
        with self.assertRaisesRegex(ValueError, 'unsafe.*path'):
            CHECKER.candidate(official=True)

    def test_inventory_mismatch_refused(self):
        path = self.manifest['inventory']
        inventory = self.root / 'vendor' / path
        inventory.write_text('[]')
        with self.assertRaisesRegex(ValueError, 'inventory'):
            CHECKER.candidate(official=True)

    def test_archive_traversal_refused_before_read(self):
        archive = self.root / 'vendor' / self.manifest['archive']
        with tarfile.open(archive, 'w:gz') as tar:
            tar.addfile(tarfile.TarInfo('../escape'))
        self.manifest['archive_sha256'] = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.save_manifest()
        with self.assertRaisesRegex(ValueError, 'unsafe source archive member'):
            CHECKER.candidate(official=True)

    def test_inconsistent_build_provenance_refused(self):
        record = self.root / 'vendor' / self.manifest['build_record']
        data = json.loads(record.read_text())
        data['source_revision'] = '0' * 40
        record.write_text(json.dumps(data))
        self.manifest['build_record_sha256'] = hashlib.sha256(record.read_bytes()).hexdigest()
        self.save_manifest()
        with self.assertRaisesRegex(ValueError, 'build record provenance mismatch'):
            CHECKER.candidate(official=True)

    def test_bundle_source_disagreement_refused(self):
        inventory = json.loads((self.root / 'vendor' / self.manifest['inventory']).read_text())
        expected = {item['path']: item['sha256'] for item in inventory}
        expected['Cargo.toml'] = '0' * 64
        with self.assertRaisesRegex(ValueError, 'bundle committed source disagrees'):
            CHECKER.verify_bundle(self.manifest, expected)

    def test_official_overrides_refused(self):
        for name in ('AQ_GIT_BIN', 'AQ_GIT_SOURCE_DIR'):
            with self.subTest(name=name), patch.dict(os.environ, {name: '/local/override'}):
                with self.assertRaisesRegex(ValueError, 'reject local'):
                    CHECKER.candidate(official=True)

    def test_docker_stage_drift_refused(self):
        dockerfile = self.root / 'Dockerfile.worker'
        dockerfile.write_text(dockerfile.read_text().replace('WORKDIR /src', 'WORKDIR /drift', 1))
        with self.assertRaisesRegex(ValueError, 'drifted'):
            CHECKER.candidate(official=True)


if __name__ == '__main__':
    unittest.main()
