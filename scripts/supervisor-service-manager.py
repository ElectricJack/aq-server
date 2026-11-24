#!/usr/bin/env python3
"""Run the scoped central fleet. Private config, Docker access, no native agent.

Creates only labelled, per-conversation containers. It neither prunes containers
nor changes worker services. Every supervisor reads and refreshes one shared
ChatGPT login (a volume at /login); existing containers keep their durable host
state. A stopped supervisor retains its conversation.
"""
import io
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tarfile
import time

import uuid

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
    if not value.startswith('sha256:') or len(value) != 71:
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
        self.login_volume = self.prefix + 'login'
        if cfg['service'].get('secret_file') != '/login/auth.json':
            raise ValueError('shared supervisor login required')

    def enabled_scopes(self):
        query = 'SELECT conversation_id FROM supervisor_controls WHERE enabled ORDER BY conversation_id'
        output = run(['docker', 'exec', '-i', self.cfg['postgres_container'], 'sh', '-c',
                      'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; '
                      'exec psql -h 127.0.0.1 -U agent_q -d agent_q -At'], query.encode())
        return output.decode().splitlines()

    def reconcile(self):
        try:
            for scope in self.enabled_scopes():
                self.ensure(scope)
        except Exception:
            # Subprocess/config exceptions can contain private connection data.
            print('Supervisor fleet reconciliation needs inspection; retained containers unchanged', flush=True)

    def ensure(self, scope):
        if str(uuid.UUID(scope)) != scope:
            raise ValueError('scope identity')
        name = self.prefix + scope
        found = run(['docker', 'container', 'ls', '-aq', '--filter', 'name=^/' + name + '$'])
        if found.decode().strip():
            self.inspect_retained(name, scope)
            return
        self.create(name, scope)

    def inspect_retained(self, name, scope):
        record = json.loads(run(['docker', 'inspect', name]))[0]
        mounts = {mount.get('Destination'): mount.get('Name') for mount in record.get('Mounts', [])}
        owns_scope = record['Config']['Labels'].get('io.agent-q.supervisor.scope') == scope
        same_deployment = record['Image'] == self.image and mounts.get('/login') == self.login_volume
        if not owns_scope or not same_deployment:
            raise ValueError('retained supervisor needs explicit upgrade')
        if not record['State']['Running']:
            # A dead PID namespace may retain an uncertain generation.
            print('Retained supervisor is stopped: ' + scope, flush=True)

    def create(self, name, scope):
        service = dict(self.cfg['service'], conversation_id=scope)
        volume = name + '-state'
        run(['docker', 'volume', 'create', '--label', 'io.agent-q.supervisor.scope=' + scope, volume])
        run(['docker', 'create', '--name', name, '--label', 'io.agent-q.supervisor.scope=' + scope,
             '--network', self.cfg['network'], '--read-only', '--tmpfs', '/tmp:rw,mode=1777',
             '--tmpfs', '/native-homes:rw,mode=0711', '--cpus', '0.5', '--memory', '2g', '--memory-swap', '2g',
             '--pids-limit', '512', '--stop-timeout', '30', '--ulimit', 'core=0', '--security-opt', 'no-new-privileges',
             '--mount', 'type=volume,src=' + volume + ',dst=/state',
             '--mount', 'type=volume,src=' + name + '-workspace,dst=/workspace',
             '--mount', 'type=volume,src=' + self.login_volume + ',dst=/login',
             '--log-opt', 'max-size=2m', '--log-opt', 'max-file=2',
             self.image, 'serve', '/state/service.json'])
        files = {'service.json': json.dumps(service).encode()}
        self.seed_login(name)
        run(['docker', 'cp', '-', name + ':/state'], archive(files))
        if self.cfg.get('egress_network'):
            run(['docker', 'network', 'connect', self.cfg['egress_network'], name])
        run(['docker', 'start', name])
        print('Created supervisor: ' + scope, flush=True)

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
    fleet = Fleet(json.loads(private(Path(sys.argv[1]))))
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    while not stopping:
        fleet.reconcile()
        for _ in range(20):
            if stopping:
                break
            time.sleep(0.1)
    # Controller services remain alive and retain their independent lifecycle;
    # use the dashboard Stop command for a confirmed native stop.


if __name__ == '__main__':
    main()
