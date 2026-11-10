#!/usr/bin/env python3
"""Run the scoped central fleet. Private config, Docker access, no native agent.

Creates only labelled, per-conversation containers. It neither prunes containers
nor changes worker services. Existing containers keep their own subscription
cache and durable host state. A stopped supervisor retains its conversation.
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


def run(args, data=None):
    return subprocess.run(args, input=data, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, check=True).stdout


def private(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_uid != os.geteuid():
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


def main():
    cfg = json.loads(private(Path(sys.argv[1])))
    image = cfg['image']
    if not image.startswith('sha256:') or len(image) != 71:
        raise ValueError('immutable image required')
    prefix = cfg['prefix']
    if not prefix.startswith('aq-ms1-supervisor-') or not prefix.replace('-', '').isalnum():
        raise ValueError('scoped namespace required')
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    while not stopping:
        try:
            query = 'SELECT conversation_id FROM supervisor_controls WHERE enabled ORDER BY conversation_id'
            scopes = run(['docker', 'exec', '-i', cfg['postgres_container'], 'sh', '-c',
                          'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; '
                          'exec psql -h 127.0.0.1 -U agent_q -d agent_q -At'], query.encode()).decode().splitlines()
            for scope in scopes:
                import uuid
                if str(uuid.UUID(scope)) != scope:
                    raise ValueError('scope identity')
                name = prefix + scope
                found = run(['docker', 'container', 'ls', '-aq', '--filter', 'name=^/' + name + '$']).decode().strip()
                if found:
                    record = json.loads(run(['docker', 'inspect', name]))[0]
                    if record['Config']['Labels'].get('io.agent-q.supervisor.scope') != scope or record['Image'] != image:
                        raise ValueError('retained supervisor needs explicit upgrade')
                    if not record['State']['Running']:
                        # No blind restart: a dead PID namespace can leave an
                        # uncertain generation. Retain it for operator inspection.
                        print('Retained supervisor is stopped: ' + scope, flush=True)
                    continue
                service = dict(cfg['service'], conversation_id=scope)
                auth = private(Path(cfg['subscription_file']))
                parsed = json.loads(auth)
                if parsed.get('auth_mode') != 'chatgpt' or parsed.get('OPENAI_API_KEY'):
                    raise ValueError('subscription authentication required')
                volume = name + '-state'
                run(['docker', 'volume', 'create', '--label', 'io.agent-q.supervisor.scope=' + scope, volume])
                run(['docker', 'create', '--name', name, '--label', 'io.agent-q.supervisor.scope=' + scope,
                     '--network', cfg['network'], '--cpus', '0.5', '--memory', '2g', '--memory-swap', '2g',
                     '--pids-limit', '512', '--ulimit', 'core=0', '--security-opt', 'no-new-privileges',
                     '--mount', 'type=volume,src=' + volume + ',dst=/state',
                     '--log-opt', 'max-size=2m', '--log-opt', 'max-file=2',
                     image, 'serve', '/state/service.json'])
                run(['docker', 'cp', '-', name + ':/state'], archive({'service.json': json.dumps(service).encode(), 'subscription.json': auth}))
                if cfg.get('egress_network'):
                    run(['docker', 'network', 'connect', cfg['egress_network'], name])
                run(['docker', 'start', name])
                print('Created supervisor: ' + scope, flush=True)
        except Exception:
            # Subprocess/config exceptions can contain private connection data.
            print('Supervisor fleet reconciliation needs inspection; retained containers unchanged', flush=True)
        for _ in range(20):
            if stopping:
                break
            time.sleep(0.1)
    # Controller services remain alive and retain their independent lifecycle;
    # use the dashboard Stop command for a confirmed native stop.


if __name__ == '__main__':
    main()
