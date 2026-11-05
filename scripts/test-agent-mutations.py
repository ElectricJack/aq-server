#!/usr/bin/env python3
"""Qualify AQ-07 against API-01's pinned disposable public graph fixture.
No production database or graph, image pulls, fixture rewriting or daemon starts.
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
            env = dict(os.environ, CARGO_INCREMENTAL='0', AQ_MUTATION_GRAPH_URL=base,
                       AQ_MUTATION_GRAPH_TOKEN_FILE=str(writer_file))
            subprocess.run(['cargo', 'test', '--locked', '--offline', '-p', 'aq-server', '--test', 'graph_mutations',
                            'actual_qualified_graph_delivery_recovers_lost_receipt_without_duplicate_history',
                            '--', '--ignored', '--nocapture'], cwd=ROOT.parents[2], env=env, check=True)'''
if source.count(original) != 1:
    raise SystemExit('fixture harness changed; review AQ-07 wrapper before running')
namespace = {'__file__': str(fixture), '__name__': 'aq07_qualified_fixture'}
exec(compile(source.replace(original, replacement), str(fixture), 'exec'), namespace)
namespace['main']()
