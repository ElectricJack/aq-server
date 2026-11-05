import json
from pathlib import Path
import shutil
import subprocess

root = Path.cwd()
context = root / '.aq/tmp/e1b/bundle-image'
(context / 'vendor').mkdir(parents=True, exist_ok=True)
pin = json.loads((root / 'vendor/dashboard-candidate.json').read_text())
for file in [pin['archive'], 'dashboard-candidate.json']:
    shutil.copyfile(root / 'vendor' / file, context / 'vendor' / file)
server = (root / 'Dockerfile').read_text().split('FROM runtime AS server\n', 1)[1]
fragment = server[server.index('COPY vendor/' + pin['archive']):server.index('ENV AQ_VAULT_DIR=/vault')]
(context / 'Dockerfile').write_text('FROM debian:bookworm-slim\n' + fragment + '\nUSER 10001:10001\nCMD ["sha256sum", "/usr/share/agent-q/dashboard/dashboard-manifest.json"]\n')
tag = 'aqv2-e1b-dashboard-pin:smart-falcon-16.3'
subprocess.run(['docker', 'build', '--network=none', '-t', tag, str(context)], check=True)
result = subprocess.run(['docker', 'run', '--rm', '--read-only', '--network=none', '--cap-drop=ALL', '--security-opt=no-new-privileges:true', tag], text=True, stdout=subprocess.PIPE, check=True)
assert 'sha256:' + result.stdout.split()[0] == pin['dashboard_digest'], result.stdout
print(result.stdout.strip())
subprocess.run(['docker', 'image', 'inspect', tag, '--format', '{{.Id}} {{json .Config.Env}} {{.Config.User}}'], check=True)
subprocess.run(['docker', 'image', 'rm', tag], check=True)
print('PASS: actual production dashboard COPY/RUN/ENV fragment verified and readable by UID 10001 in a read-only container; complete server image was not built.')
