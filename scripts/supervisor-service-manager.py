#!/usr/bin/env python3
"""Run the scoped central fleet. Private config, Docker access, no native agent.

Creates only labelled, per-conversation containers. It neither prunes containers
nor changes worker services. Every supervisor reads and refreshes one shared
ChatGPT login (a volume at /login); existing containers keep their durable host
state. A stopped supervisor retains its conversation.
"""
import argparse
from datetime import datetime
import fcntl
import io
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import tarfile
import time

import uuid

SCOPE_LABEL = 'io.agent-q.supervisor.scope'
CLAUDE_NATIVE = '/opt/claude/claude'
CLAUDE_LOGIN = '/claude-login'

def run(args, data=None):
    return subprocess.run(args, input=data, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, check=True).stdout


def private(path):
    info = path.lstat()
    owned = info.st_uid == os.geteuid() and not info.st_mode & 0o077
    if not owned or not stat.S_ISREG(info.st_mode):
        raise ValueError('private manager file required')
    return path.read_bytes()


def archive(files):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode='w') as tar:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.mode, info.uid, info.gid, info.size = 0o600, 0, 0, len(data)
            tar.addfile(info, io.BytesIO(data))
    return out.getvalue()


def immutable_image(value):
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', value):
        raise ValueError('immutable image required')
    return value


def scoped_prefix(value):
    if not value.startswith('aq-ms1-supervisor-') or not value.replace('-', '').isalnum():
        raise ValueError('scoped namespace required')
    return value


class Fleet:
    def __init__(self, cfg):
        self.cfg = cfg
        self.image = immutable_image(cfg['image'])
        self.prefix = scoped_prefix(cfg['prefix'])
        # A refresh rotates the refresh token, so per-container copies lock each
        # other out. One login file for the fleet: the host's flock on its
        # directory serializes copy-backs and each turn starts from the newest.
        # shortcut: two turns refreshing in the same seconds can still collide;
        # upgrade to refreshing under the lock if that is seen.
        codex = ('native', 'native_digest', 'secret_file')
        configured = [key for key in codex if cfg['service'].get(key) is not None]
        self.login_volume = None
        if configured:
            if len(configured) != len(codex) or cfg['service']['secret_file'] != '/login/auth.json':
                raise ValueError('complete Codex configuration and shared supervisor login required')
            self.login_volume = self.prefix + 'login'

        self.claude_dir = None
        claude = cfg['service'].get('claude')
        if claude is not None:
            if (set(claude) != {'native', 'native_digest', 'secret_file'}
                    or claude['native'] != CLAUDE_NATIVE
                    or claude['secret_file'] != CLAUDE_LOGIN + '/token'
                    or not re.fullmatch(r'[0-9a-f]{64}', claude['native_digest'])):
                raise ValueError('pinned Claude supervisor configuration required')
            directory = Path(cfg['claude_subscription_dir'])
            if not directory.is_absolute() or any(c in str(directory) for c in ',\"\n\r'):
                raise ValueError('absolute Claude subscription directory required')
            if directory != directory.resolve(strict=True):
                raise ValueError('use the canonical Claude subscription directory path')
            self.claude_dir = directory
            self.validate_claude_login()
        elif 'claude_subscription_dir' in cfg:
            raise ValueError('Claude subscription directory requires service.claude')
        if self.login_volume is None and self.claude_dir is None:
            raise ValueError('at least one supervisor harness is required')

    def validate_claude_login(self):
        info = self.claude_dir.stat()
        if info.st_uid != 0 or not stat.S_ISDIR(info.st_mode) or info.st_mode & 0o077:
            raise ValueError('root-owned private Claude subscription directory required')
        token = self.claude_dir / 'token'
        info = token.lstat()
        if info.st_uid != 0 or not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_size > 65536:
            raise ValueError('root-owned private Claude setup token required')
        value = token.read_bytes().rstrip(b'\r\n')
        if not value or not all(33 <= byte <= 126 for byte in value):
            raise ValueError('one-line Claude setup token required')

    def query(self, query):
        output = run(['docker', 'exec', '-i', self.cfg['postgres_container'], 'sh', '-c',
                      'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; '
                      'exec psql -h 127.0.0.1 -U agent_q -d agent_q -At -v ON_ERROR_STOP=1'], query.encode())
        return output.decode().splitlines()

    def enabled_scopes(self):
        return self.query('SELECT conversation_id FROM supervisor_controls WHERE enabled ORDER BY conversation_id')

    def stopped_at(self, scope):
        rows = self.query(
            "SELECT r.observed_at_ms FROM supervisor_controls c JOIN supervisor_service_reports r USING(conversation_id) "
            f"WHERE NOT c.enabled AND c.conversation_id = '{scope}' "
            "AND r.phase = 'stopped' AND r.observed_at_ms >= c.updated_at_ms"
        )
        return int(rows[0]) if len(rows) == 1 else None

    def require_disabled(self, scope):
        if self.query(f"SELECT enabled FROM supervisor_controls WHERE conversation_id = '{scope}'") != ['f']:
            raise ValueError('disable the supervisor before upgrading')

    def require_stopped(self, scope):
        if self.stopped_at(scope) is None:
            raise ValueError('disable the supervisor and wait for its stopped report before upgrading')

    def reconcile(self):
        try:
            scopes = self.enabled_scopes()
        except Exception:
            print('Supervisor fleet could not read enabled conversations', flush=True)
            return
        for scope in scopes:
            try:
                self.ensure(scope)
            except Exception:
                # Subprocess/config exceptions can contain private connection data.
                print('Supervisor fleet reconciliation needs inspection; retained containers unchanged', flush=True)

    def scope_name(self, scope):
        if str(uuid.UUID(scope)) != scope or uuid.UUID(scope).int == 0:
            raise ValueError('scope identity')
        return self.prefix + scope

    def container(self, name):
        found = run(['docker', 'container', 'ls', '-aq', '--filter', 'name=^/' + name + '$']).splitlines()
        if not found:
            return None
        if len(found) != 1:
            raise ValueError('ambiguous retained container')
        record = json.loads(run(['docker', 'inspect', found[0].decode()]))[0]
        if record['Name'] != '/' + name:
            raise ValueError('retained container name changed')
        return record

    def require_owned(self, record, name, scope):
        mounts = {m['Destination']: m for m in record['Mounts']}
        if (record['Config']['Labels'] or {}).get(SCOPE_LABEL) != scope:
            raise ValueError('retained supervisor has a different scope')
        for target, suffix in [('/state', '-state'), ('/workspace', '-workspace')]:
            mount = mounts.get(target, {})
            if mount.get('Type') != 'volume' or mount.get('Name') != name + suffix or not mount.get('RW'):
                raise ValueError('retained supervisor has unexpected durable mounts')
        return mounts

    def matches_deployment(self, record, mounts):
        if record['Image'] != self.image:
            return False
        login = mounts.get('/login')
        if self.login_volume is None:
            if login is not None:
                return False
        elif login is None or login.get('Name') != self.login_volume or login.get('Type') != 'volume' or not login.get('RW'):
            return False
        claude = mounts.get(CLAUDE_LOGIN)
        if self.claude_dir is None:
            return claude is None
        return (claude is not None and claude['Type'] == 'bind' and not claude['RW']
                and claude['Source'] == str(self.claude_dir))

    def ensure(self, scope):
        name = self.scope_name(scope)
        record = self.container(name)
        if self.container(name + '-upgrade-backup'):
            raise ValueError('finish interrupted supervisor upgrade first')
        if record:
            mounts = self.require_owned(record, name, scope)
            if not self.matches_deployment(record, mounts):
                raise ValueError('retained supervisor needs explicit upgrade')
            if not record['State']['Running']:
                # A dead PID namespace may retain an uncertain generation.
                print('Retained supervisor is stopped: ' + scope, flush=True)
            return
        self.create(name, scope)

    def upgrade(self, scope):
        name = self.scope_name(scope)
        backup_name = name + '-upgrade-backup'
        self.require_disabled(scope)
        if self.claude_dir:
            self.validate_claude_login()
        run(['docker', 'image', 'inspect', self.image])

        record = self.container(name)
        backup = self.container(backup_name)
        if backup:
            self.require_owned(backup, name, scope)
            if backup['State']['Status'] != 'exited' or backup['State']['ExitCode'] != 0:
                raise ValueError('upgrade backup lacks a clean exit')
            if record:
                mounts = self.require_owned(record, name, scope)
                if not self.matches_deployment(record, mounts):
                    raise ValueError('retry with the same supervisor deployment')
                if not record['State']['Running']:
                    if record['State']['Status'] != 'created':
                        raise ValueError('replacement exited; inspect it before retrying')
                    self.initialize(record['Id'], scope, record)
            else:
                self.create(name, scope)
        else:
            if not record:
                raise ValueError('retained supervisor is required for upgrade')
            self.require_owned(record, name, scope)
            self.require_stopped(scope)
            if record['State']['Running']:
                run(['docker', 'stop', '--timeout', '30', record['Id']])
                record = self.container(name)
            if record['State']['Status'] != 'exited' or record['State']['ExitCode'] != 0:
                raise ValueError('retained controller needs a clean exit before upgrade')
            self.require_stopped(scope)
            run(['docker', 'rename', record['Id'], backup_name])
            backup = record
            self.create(name, scope)

        # Keep the backup until the new controller has acquired the scope and
        # reported a stopped phase of its own.
        self.wait_for_replacement(name, scope)
        run(['docker', 'rm', backup['Id']])
        print('Upgraded supervisor: ' + scope, flush=True)

    def wait_for_replacement(self, name, scope):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            record = self.container(name)
            if not record or not record['State']['Running']:
                raise ValueError('replacement is not running; inspect retained containers')
            started = datetime.fromisoformat(record['State']['StartedAt'])
            reported = self.stopped_at(scope)
            if reported is not None and reported >= int(started.timestamp() * 1000):
                return
            time.sleep(0.5)
        raise ValueError('replacement did not report stopped; leave the scope disabled and retry')

    def create(self, name, scope):
        volume = name + '-state'
        run(['docker', 'volume', 'create', '--label', SCOPE_LABEL + '=' + scope, volume])
        cmd = ['docker', 'create', '--name', name, '--label', SCOPE_LABEL + '=' + scope,
               '--network', self.cfg['network'], '--read-only', '--tmpfs', '/tmp:rw,mode=1777',
               '--tmpfs', '/native-homes:rw,mode=0711', '--cpus', '0.5', '--memory', '2g', '--memory-swap', '2g',
               '--pids-limit', '512', '--stop-timeout', '30', '--ulimit', 'core=0', '--security-opt', 'no-new-privileges',
               '--mount', 'type=volume,src=' + volume + ',dst=/state',
               '--mount', 'type=volume,src=' + name + '-workspace,dst=/workspace',
               '--log-opt', 'max-size=2m', '--log-opt', 'max-file=2']
        if self.login_volume:
            cmd.extend(['--mount', 'type=volume,src=' + self.login_volume + ',dst=/login'])
        if self.claude_dir:
            self.validate_claude_login()
            cmd.extend(['--mount', f'type=bind,src={self.claude_dir},dst={CLAUDE_LOGIN},readonly'])
        run(cmd + [self.image, 'serve', '/state/service.json'])
        self.initialize(name, scope, self.container(name))
        print('Created supervisor: ' + scope, flush=True)

    def initialize(self, target, scope, record):
        service = dict(self.cfg['service'], conversation_id=scope, image_digest=self.image)
        if self.login_volume:
            self.seed_login(target)
        run(['docker', 'cp', '-', target + ':/state'], archive({'service.json': json.dumps(service).encode()}))
        egress = self.cfg.get('egress_network')
        if egress and egress not in (record['NetworkSettings']['Networks'] or {}):
            run(['docker', 'network', 'connect', egress, target])
        run(['docker', 'start', target])

    def seed_login(self, name):
        # Discard the read. The shared volume may hold a newer credential.
        probe = subprocess.run(['docker', 'cp', name + ':/login/auth.json', '-'],
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if probe.returncode == 0:
            return
        auth = private(Path(self.cfg['subscription_file']))
        parsed = json.loads(auth)
        if parsed.get('auth_mode') != 'chatgpt' or parsed.get('OPENAI_API_KEY'):
            raise ValueError('subscription authentication required')
        run(['docker', 'cp', '-', name + ':/login'], archive({'auth.json': auth}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', type=Path)
    parser.add_argument('command', nargs='?', choices=['upgrade'])
    parser.add_argument('scope', nargs='?')
    args = parser.parse_args()
    if (args.command is None) != (args.scope is None):
        parser.error('upgrade requires exactly one conversation UUID')
    config = args.config.absolute()
    lock_path = config.with_name(config.name + '.lock')
    lock_fd = os.open(lock_path, os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_fd) as lock:
        if args.command == 'upgrade':
            fcntl.flock(lock, fcntl.LOCK_EX)
            try:
                Fleet(json.loads(private(config))).upgrade(args.scope)
            except ValueError:
                # Only report explicitly safe operational status.
                print('Supervisor upgrade needs inspection; check scope, stopped report, deployment and mounts', flush=True)
                raise SystemExit(1) from None
            except (OSError, KeyError, subprocess.CalledProcessError):
                print('Supervisor upgrade interrupted; inspect retained containers before retrying', flush=True)
                raise SystemExit(1) from None
        else:
            manage(Fleet(json.loads(private(config))), lock)


def manage(fleet, lock):
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    while not stopping:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            fleet.reconcile()
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)
        for _ in range(20):
            if stopping:
                break
            time.sleep(0.1)
    # Controller services remain alive and retain their independent lifecycle;
    # use the dashboard Stop command for a confirmed native stop.


if __name__ == '__main__':
    main()
