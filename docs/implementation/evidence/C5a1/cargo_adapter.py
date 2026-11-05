import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path


def test_cargo():
    command = json.loads(os.environ['C5A1_COMMAND'])
    log = Path(os.environ['C5A1_WORKDIR']) / '.aq/tmp/c5a1' / (hashlib.sha256(json.dumps(command).encode()).hexdigest()[:12] + '.command.log')
    with log.open('w') as output:
        result = subprocess.run(command, cwd=os.environ['C5A1_WORKDIR'], stdout=output, stderr=subprocess.STDOUT, check=False)
    sys.stderr.write(log.read_text())
    assert result.returncode == 0, f'{command} exited {result.returncode}; {log}'
