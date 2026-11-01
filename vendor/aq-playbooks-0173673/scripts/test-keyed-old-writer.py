#!/usr/bin/env python3
"""Probe old0.1.0 from its exact Git archive against a disposable new writer.
Requires AQ_PLAYBOOKS_TEST_DATABASE_URL to name an empty aq_pb_keyed_* database
on scripts/test-db loopback55432. No live sibling checkout or network discovery.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import tarfile
import tempfile
from urllib.parse import urlparse

parser = argparse.ArgumentParser()
parser.add_argument('--old-source-archive', type=Path, required=True)
args = parser.parse_args()
archive = args.old_source_archive.read_bytes()
archive_hash = hashlib.sha256(archive).hexdigest()
assert archive_hash == 'ad516d7cc5f5bded424aa364f7185db2049a1ac21b15b767c279e906ce829c9e'
url = urlparse(os.environ['AQ_PLAYBOOKS_TEST_DATABASE_URL'])
assert url.hostname == '127.0.0.1' and url.port == 55432 and url.path.startswith('/aq_pb_keyed_')
root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='pb-keyed-old-') as temp:
    base = Path(temp)
    old = base / 'old'
    old.mkdir()
    with tarfile.open(args.old_source_archive) as tar:
        tar.extractall(old, filter='data')
    probe = base / 'probe'
    (probe / 'src').mkdir(parents=True)
    dependencies = ''.join(name + '={path=' + json.dumps(str(old / 'crates' / name)) + ',version="=0.1.0"}\n' for name in ['aq-playbooks-postgres', 'aq-playbooks-runner'])
    (probe / 'Cargo.toml').write_text('[package]\nname="pb-old-writer-probe"\nversion="0.0.0"\nedition="2024"\n[dependencies]\n' + dependencies)
    (probe / 'src/main.rs').write_text(r'''
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{Engine,Storage};
use std::io::{self,Write};
fn main(){
 let url=std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL").unwrap();
 let engine=Engine::new(PostgresStorage::connect(&url).unwrap(),"legacy-probe","old-owner").unwrap();
 engine.release().unwrap();println!("OLD_CONNECTION_READY");io::stdout().flush().unwrap();
 let mut line=String::new();io::stdin().read_line(&mut line).unwrap();
 assert!(engine.store.acquire("legacy-probe","old-owner").is_err());println!("WARM_OLD_REFUSED");
 assert!(PostgresStorage::connect(&url).is_err());println!("COLD_OLD_REFUSED");
}
''')
    subprocess.run(['cargo', 'build', '--offline', '--manifest-path', str(probe / 'Cargo.toml')], check=True)
    executable = Path(os.environ.get('CARGO_TARGET_DIR', str(probe / 'target'))) / 'debug/pb-old-writer-probe'
    child = subprocess.Popen([str(executable)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert select.select([child.stdout], [], [], 30)[0], 'old helper readiness timeout'
        assert child.stdout.readline().strip() == 'OLD_CONNECTION_READY'
        result = subprocess.run(['cargo', 'run', '--locked', '--offline', '-q', '-p', 'aq-playbooks-postgres', '--example', 'keyed_receipt_probe', '--', 'legacy-probe'], cwd=root, check=True, capture_output=True, text=True, timeout=120)
        receipt = json.loads(result.stdout)
        assert receipt['key'] == 'retained-key'
        stdout, _ = child.communicate('resume\n', timeout=30)
        assert child.returncode == 0 and stdout.splitlines() == ['WARM_OLD_REFUSED', 'COLD_OLD_REFUSED'], stdout
        print(json.dumps({'result': 'passed', 'old_source_revision': '42f944996eafad5af07d9ec1c72d3e3f2630c03b', 'old_archive_sha256': archive_hash, 'old_lock_sha256': hashlib.sha256((probe / 'Cargo.lock').read_bytes()).hexdigest(), 'warm_old_connection': 'refused', 'cold_old_constructor': 'refused', 'new_receipt': receipt}, sort_keys=True))
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
