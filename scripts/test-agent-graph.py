#!/usr/bin/env python3
"""Run AQ-06 adapter against API-01's disposable qualified graph fixture.
Reuses the audited isolated-image setup/seed/cleanup without rewriting fixtures.
Needs DATABASE_URL, Docker, preloaded selected graph/postgres images, and cargo.
"""
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1]
fixture = root / 'protocol/agent-access/v1/capture_graph_fixture.py'
source = fixture.read_text()
original = "            (ROOT / 'fixtures/qualified-graph.json').write_text(json.dumps(evidence, indent=2) + '\\n')"
replacement = '''            writer_file = Path(temp) / 'ordinary-writer-token'
            writer_file.write_text(writer)
            writer_file.chmod(0o600)
            env = dict(os.environ, CARGO_INCREMENTAL='0', AQ_AGENT_GRAPH_TEST_URL=base,
                       AQ_AGENT_GRAPH_TEST_TOKEN_FILE=str(writer_file))
            subprocess.run(['cargo', 'test', '-p', 'aq-server', '--test', 'agent_access',
                            '--', '--nocapture'], cwd=ROOT.parents[2], env=env, check=True)'''
if source.count(original) != 1:
    raise SystemExit('fixture harness changed; review AQ-06 wrapper before running')
namespace = {'__file__': str(fixture), '__name__': 'aq06_qualified_fixture'}
exec(compile(source.replace(original, replacement), str(fixture), 'exec'), namespace)
namespace['main']()
