#!/usr/bin/env python3
"""Explicit AQ-19 qualification against retained aq-git and qualified aq-graph.
Creates synthetic local Docker resources only; never pulls/publishes images or
writes an operator repository, credentials, graph workspace or API-01 fixtures.
Requires DATABASE_URL from scripts/test-db and the preloaded selected images.
"""
import importlib.util
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
GIT_IMAGE = 'aq-git-a3b:0.2.0'
GIT_IMAGE_ID = 'sha256:4e405e60f637f766582fcb182d1e3bd963ba8e78c94d2cc57203d28f0f358642'
fixture = ROOT / 'protocol/agent-access/v1/capture_graph_fixture.py'
source = fixture.read_text()
original = "            (ROOT / 'fixtures/qualified-graph.json').write_text(json.dumps(evidence, indent=2) + '\\n')"
replacement = '''            import socket
            git_name = prefix + '-git'
            git_data = Path(temp) / 'project-git'
            git_data.mkdir(mode=0o700)
            git_socket = git_data / 'aq-git.sock'
            writer_file = Path(temp) / 'ordinary-project-token'
            writer_file.write_text(writer)
            writer_file.chmod(0o600)
            repository = uid(207)
            assert run('docker', 'image', 'inspect', GIT_IMAGE, '--format', '{{.Id}}') == GIT_IMAGE_ID
            try:
                run('docker', 'run', '-d', '--rm', '--name', git_name,
                    '--user', f'{os.getuid()}:{os.getgid()}',
                    '--mount', f'type=bind,src={git_data},dst=/data',
                    GIT_IMAGE, '--state-dir', '/data/state', '--listen', '/data/aq-git.sock', 'serve')
                for _ in range(200):
                    if git_socket.exists():
                        break
                    time.sleep(0.025)
                else:
                    raise RuntimeError('selected aq-git socket did not become ready')
                def exchange(request):
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as peer:
                        peer.settimeout(5)
                        peer.connect(str(git_socket))
                        peer.sendall(json.dumps(request).encode() + b'\\n')
                        data = bytearray()
                        while not data.endswith(b'\\n'):
                            chunk = peer.recv(4096)
                            if not chunk or len(data) + len(chunk) > 65536:
                                raise RuntimeError('invalid bounded aq-git qualification response')
                            data.extend(chunk)
                        response = json.loads(data)
                        if response.get('protocol_version') != 1 or response.get('request_id') != request['request_id'] or response.get('ok') is not True:
                            raise RuntimeError('selected public aq-git operation refused')
                        return response['result']
                capabilities = exchange({'protocol_version': 1, 'request_id': 'project-capabilities', 'operation': 'capabilities', 'params': {}})
                exchange({'protocol_version': 1, 'request_id': 'project-register', 'operation': 'repo.register',
                          'repository_id': repository, 'operation_id': uid(208), 'expected_binding_generation': 0,
                          'params': {'binding': {'identity': 'project-qualification', 'mode': 'central',
                                                'allowed_refs': [], 'allowed_targets': []}}, 'deadline_ms': 5000})
                env = dict(os.environ, CARGO_INCREMENTAL='0', AQ_AGENT_GRAPH_TEST_URL=base,
                           AQ_AGENT_GRAPH_TEST_TOKEN_FILE=str(writer_file),
                           AQ_PROJECT_GIT_SOCKET=str(git_socket), AQ_PROJECT_REPOSITORY_ID=repository,
                           AQ_PROJECT_WORKSPACE_ID=workspace)
                subprocess.run(['cargo', 'test', '--locked', '-p', 'aq-server', '--test', 'project_bindings',
                                'selected_real_public_repository_and_graph_workspace_bind', '--', '--ignored', '--nocapture'],
                               cwd=ROOT.parents[2], env=env, check=True)
                print(json.dumps({'qualification': 'AQ-19', 'aq_git_image_id': GIT_IMAGE_ID,
                                  'aq_graph_image_id': IMAGE_ID, 'repository_binding_generation': 1,
                                  'method': 'Public repo.register/repo.show and workspace-scoped graph API1; synthetic resources only.'}))
            finally:
                subprocess.run(['docker', 'rm', '-fv', git_name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
'''
if source.count(original) != 1:
    raise SystemExit('qualified graph fixture changed; review the AQ-19 wrapper')
# Verify retained source checksums and the canonical build recipe before using its image.
spec = importlib.util.spec_from_file_location('git_candidate', ROOT / 'scripts/check-git-candidate.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.candidate(official=True)
namespace = {'__file__': str(fixture), '__name__': 'aq19_qualified_fixture',
             'GIT_IMAGE': GIT_IMAGE, 'GIT_IMAGE_ID': GIT_IMAGE_ID}
exec(compile(source.replace(original, replacement), str(fixture), 'exec'), namespace)
namespace['main']()
