import os, shlex, subprocess
from pathlib import Path

def test_cargo_delivery():
    env = dict(os.environ, CARGO_BUILD_JOBS='2', RUST_TEST_THREADS='2')
    command = shlex.split(os.environ['D1A_CHECK'])
    log = Path(os.environ.get('D1A_CARGO_LOG', '/tmp/d1a-cargo.log'))
    with log.open('w') as output:
        output.write('$ ' + shlex.join(command) + '\n')
        output.flush()
        result = subprocess.run(command, env=env, stdout=output, stderr=subprocess.STDOUT, text=True)
    print(log.read_text())
    result.check_returncode()
