import os
import shlex
import subprocess
from pathlib import Path

def test_e1b_cargo():
    command = shlex.split(os.environ['E1B_COMMAND'])
    output = Path(os.environ['E1B_LOG'])
    env = dict(os.environ, CARGO_BUILD_JOBS='2', RUST_TEST_THREADS='2', DATABASE_URL='postgres://postgres:test@127.0.0.1:55432/postgres')
    with output.open('w') as log:
        result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
    assert result.returncode == 0, output.read_text()[-7000:]
