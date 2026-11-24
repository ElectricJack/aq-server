#!/usr/bin/env python3
"""Exercise fleet provisioning and interrupted upgrades without a Docker daemon."""
import io
import json
from pathlib import Path
from runpy import run_path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

Fleet = run_path(str(Path(__file__).with_name('supervisor-service-manager.py')))['Fleet']
SCOPE = '836bd70d-f570-4876-b3ca-5e94c16fc929'
IMAGE = 'sha256:' + 'a' * 64
PREFIX = 'aq-ms1-supervisor-fixture-'
CODEX = {'native': '/opt/codex/codex', 'native_digest': 'b' * 64, 'secret_file': '/login/auth.json'}
CLAUDE = {'native': '/opt/claude/claude', 'native_digest': 'c' * 64, 'secret_file': '/claude-login/token'}


class FleetTest(unittest.TestCase):
    def setUp(self):
        self.directory = self.enterContext(tempfile.TemporaryDirectory())
        self.calls = []
        self.enterContext(patch.dict(Fleet.create.__globals__, run=self.run_command))

    def run_command(self, args, data=None):
        self.calls.append((args, data))
        return b''

    def fleet(self, codex=True, claude=True):
        cfg = {'image': IMAGE, 'prefix': PREFIX, 'network': 'supervisors', 'service': {}}
        if codex:
            cfg['subscription_file'] = '/fixture/auth.json'
            cfg['service'].update(CODEX)
        if claude:
            cfg['claude_subscription_dir'] = self.directory
            cfg['service']['claude'] = CLAUDE
        with patch.object(Fleet, 'validate_claude_login'):
            fleet = Fleet(cfg)
        fleet.validate_claude_login = lambda: None
        return fleet

    def test_provisions_each_harness_combination(self):
        for codex, claude in [(True, False), (False, True), (True, True)]:
            with self.subTest(codex=codex, claude=claude):
                self.calls.clear()
                fleet = self.fleet(codex, claude)
                with (patch.object(fleet, 'container', return_value={}),
                      patch.object(fleet, 'seed_login') as seed):
                    fleet.create(PREFIX + SCOPE, SCOPE)
                cmd = next(args for args, _ in self.calls if args[:2] == ['docker', 'create'])
                self.assertEqual(any('dst=/login' in arg for arg in cmd), codex)
                self.assertEqual(any(arg.endswith('dst=/claude-login,readonly') for arg in cmd), claude)
                self.assertEqual(seed.called, codex)
                payload = next(data for args, data in self.calls if args[:2] == ['docker', 'cp'])
                with tarfile.open(fileobj=io.BytesIO(payload)) as tar:
                    self.assertEqual(tar.getnames(), ['service.json'])
                    self.assertEqual(tar.getmember('service.json').mode, 0o600)
                    service = json.load(tar.extractfile('service.json'))
                self.assertEqual(service.get('claude'), CLAUDE if claude else None)
                self.assertEqual(service.get('native'), CODEX['native'] if codex else None)
                self.assertEqual(service['conversation_id'], SCOPE)
                self.assertEqual(service['image_digest'], IMAGE)

    def test_disabled_stale_stop_is_rejected_before_moving_controller(self):
        fleet = self.fleet()
        record = {'State': {'Running': True}}
        with (patch.object(fleet, 'require_disabled'),
              patch.object(fleet, 'container', side_effect=[record, None]),
              patch.object(fleet, 'require_owned'),
              patch.object(fleet, 'stopped_at', return_value=None)):
            with self.assertRaisesRegex(ValueError, 'stopped report'):
                fleet.upgrade(SCOPE)
        self.assertFalse(any(args[1] in ['stop', 'rename', 'rm'] for args, _ in self.calls))

    def test_retries_interrupted_create_without_removing_backup_until_ready(self):
        fleet = self.fleet(codex=False)
        backup = {'Id': 'original', 'State': {'Status': 'exited', 'ExitCode': 0}}
        with (patch.object(fleet, 'container', side_effect=[None, backup, None, backup]),
              patch.object(fleet, 'require_disabled'),
              patch.object(fleet, 'require_owned'),
              patch.object(fleet, 'wait_for_replacement') as ready,
              patch.object(fleet, 'create', side_effect=[subprocess.CalledProcessError(1, []), None])):
            with self.assertRaises(subprocess.CalledProcessError):
                fleet.upgrade(SCOPE)
            self.assertFalse(any(args[1] == 'rm' for args, _ in self.calls))
            ready.assert_not_called()
            fleet.upgrade(SCOPE)
            ready.assert_called_once_with(PREFIX + SCOPE, SCOPE)
        self.assertIn((['docker', 'rm', 'original'], None), self.calls)


if __name__ == '__main__':
    unittest.main()
