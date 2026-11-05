import hashlib
import os
from pathlib import Path
import subprocess
import sys

ROOT = next(parent for parent in Path(__file__).resolve().parents if (parent / 'Cargo.toml').is_file())
LOGS = ROOT / 'docs/implementation/evidence/G6-followup'

def test_results_feedback():
    env = dict(os.environ)
    phase = env.get('G6_FOLLOWUP_PHASE', 'first')
    env.update(CARGO_BUILD_JOBS='2', RUST_TEST_THREADS='2', TOKIO_WORKER_THREADS='2',
               DATABASE_URL=env.get('DATABASE_URL', 'postgres://postgres:test@127.0.0.1:55432/postgres'))
    if phase in ('first', 'retry', 'merged'):
        binary = Path(env.get('AQ_TEST_GRAPH_BINARY', str(ROOT / '.aq/tmp/g6-followup/pinned/aq-graph')))
        expected = env.get('AQ_TEST_GRAPH_SHA256', '5d649f1a3dbcc6f1acd1b18b0a6c2a8e17eefa48cbadc43bc35083eaaae5dfd9')
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == expected, 'Graph fixture binary digest mismatch'
        env.update(AQ_TEST_GRAPH_BINARY=str(binary), AQ_TEST_GRAPH_SHA256=expected)
    commands = {
        'first': [
            ('schema-generate', ['cargo', 'run', '--locked', '-p', 'aq-server', '--example', 'results_feedback_contract']),
            ('results-feedback', ['cargo', 'test', '--locked', '--workspace', 'results_feedback']),
        ],
        'retry': [
            ('results-feedback-recheck', ['cargo', 'test', '--locked', '--workspace', 'results_feedback']),
        ],
        'wire': [
            ('wire-fixtures', [sys.executable, 'protocol/results-feedback/v1/check.py']),
            ('action-fixtures', [sys.executable, 'protocol/dashboard-actions/v1/check.py']),
            ('read-fixtures', [sys.executable, 'protocol/dashboard-read/v1/check.py']),
        ],
        'final': [
            ('fmt', ['cargo', 'fmt', '--check']),
            ('clippy', ['cargo', 'clippy', '--locked', '-p', 'aq-server', '--all-targets', '--', '-D', 'warnings']),
            ('schema-check', ['cargo', 'run', '--locked', '-p', 'aq-server', '--example', 'results_feedback_contract', '--', '--check']),
            ('dashboard-area', ['cargo', 'test', '--locked', '-p', 'aq-server', '--test', 'dashboard_read', '--test', 'dashboard_actions', '--test', 'dashboard_mounts']),
        ],
        'area-retry': [
            ('dashboard-area-recheck', ['cargo', 'test', '--locked', '-p', 'aq-server', '--test', 'dashboard_read', '--test', 'dashboard_actions', '--test', 'dashboard_mounts']),
        ],
        'merged-compat-retry': [
            ('wire-merged-recheck', [sys.executable, 'protocol/results-feedback/v1/check.py']),
            ('action-merged-recheck', [sys.executable, 'protocol/dashboard-actions/v1/check.py']),
            ('read-merged-recheck', [sys.executable, 'protocol/dashboard-read/v1/check.py']),
            ('dashboard-area-merged', ['cargo', 'test', '--locked', '-p', 'aq-server', '--test', 'dashboard_read', '--test', 'dashboard_actions', '--test', 'dashboard_mounts']),
        ],
        'merged-compat': [
            ('wire-merged', [sys.executable, 'protocol/results-feedback/v1/check.py']),
            ('action-merged', [sys.executable, 'protocol/dashboard-actions/v1/check.py']),
            ('read-merged', [sys.executable, 'protocol/dashboard-read/v1/check.py']),
            ('dashboard-area-merged', ['cargo', 'test', '--locked', '-p', 'aq-server', '--test', 'dashboard_read', '--test', 'dashboard_actions', '--test', 'dashboard_mounts']),
        ],
        'merged': [
            ('results-feedback-merged', ['cargo', 'test', '--locked', '--workspace', '--test', 'profiled_results', 'results_feedback']),
            ('decoder-merged', ['cargo', 'test', '--locked', '-p', 'aq-server', '--lib', 'results_feedback']),
            ('schema-merged', ['cargo', 'run', '--locked', '-p', 'aq-server', '--example', 'results_feedback_contract', '--', '--check']),
        ],
    }[phase]
    for name, command in commands:
        log = LOGS / f'{name}.log'
        with log.open('w') as output:
            output.write('$ ' + ' '.join(command) + '\n')
            output.flush()
            result = subprocess.run(command, cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT, timeout=1800)
        assert result.returncode == 0, f'{command} exited {result.returncode}; {log}\n{log.read_text()[-20000:]}'
