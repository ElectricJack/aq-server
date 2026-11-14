#!/usr/bin/env python3
"""Actual frozen0.1.2 warm CAS/cold constructor refusal at catalog schema4.

Requires an empty aq_pb_catalog_* database on the existing disposable loopback
55432 PostgreSQL. No canonical configuration, remote source or publication.
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
parser.add_argument("--old-source-archive", type=Path, required=True)
args = parser.parse_args()
archive_hash = hashlib.sha256(args.old_source_archive.read_bytes()).hexdigest()
assert archive_hash == "bd1246e8e972ee1627e94598945f398812c89044106b463151a696511412ca39"
url = urlparse(os.environ["AQ_PLAYBOOKS_TEST_DATABASE_URL"])
assert url.hostname == "127.0.0.1" and url.port == 55432 and url.path.startswith("/aq_pb_catalog_")
assert os.environ.get("CARGO_TARGET_DIR"), "root-assigned shared build cache required"
root = Path(__file__).resolve().parents[1]
os.environ["CARGO_INCREMENTAL"] = "0"  # bound target/ growth for every cargo child
with tempfile.TemporaryDirectory(prefix="pb-catalog-old-") as temp:
    base = Path(temp)
    old = base / "old"
    old.mkdir()
    with tarfile.open(args.old_source_archive) as source:
        source.extractall(old, filter="data")
    probe = base / "probe"
    (probe / "src").mkdir(parents=True)
    dependencies = "".join(name + "={path=" + json.dumps(str(old / "crates" / name)) + ',version="=0.1.2"}\n' for name in ["aq-playbooks-postgres", "aq-playbooks-runner"])
    (probe / "Cargo.toml").write_text('[package]\nname="pb-catalog-old-writer-probe"\nversion="0.0.0"\nedition="2024"\n[dependencies]\n' + dependencies)
    (probe / "src/main.rs").write_text(r'''
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{Engine,Storage};
use std::io::{self,Write};
fn main(){
 let url=std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL").unwrap();
 let e=Engine::new(PostgresStorage::connect(&url).unwrap(),"catalog-legacy-probe","old-owner").unwrap();
 let mut state=e.store.load(&e.namespace).unwrap();state.state.sequence=99;
 e.store.compare_and_swap(&e.namespace,&e.owner,state.revision,&state.state).unwrap();
 println!("OLD_OWNER_READY");io::stdout().flush().unwrap();
 let mut line=String::new();io::stdin().read_line(&mut line).unwrap();
 let mut state=e.store.load(&e.namespace).unwrap();state.state.sequence=100;
 assert!(e.store.compare_and_swap(&e.namespace,&e.owner,state.revision,&state.state).is_err());
 println!("WARM_OLD_CAS_REFUSED");
 assert!(PostgresStorage::connect(&url).is_err());println!("COLD_OLD_CONSTRUCTOR_REFUSED");
 // Drop the old physical connection, rather than requiring a legacy SQL
 // release operation to write a newer qualified namespace format.
}
''')
    subprocess.run(["cargo", "build", "--offline", "--manifest-path", str(probe / "Cargo.toml")], check=True)
    executable = Path(os.environ["CARGO_TARGET_DIR"]) / "debug/pb-catalog-old-writer-probe"
    child = subprocess.Popen([str(executable)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert select.select([child.stdout], [], [], 30)[0], "old owner readiness timeout"
        assert child.stdout.readline().strip() == "OLD_OWNER_READY"
        command = ["cargo", "run", "--locked", "--offline", "-q", "-p", "aq-playbooks-postgres", "--example", "catalog_compatibility_probe", "--"]
        migrated = subprocess.run(command + ["migrate", "catalog-legacy-probe"], cwd=root, check=True, capture_output=True, text=True, timeout=120)
        assert json.loads(migrated.stdout)["sequence"] == 99
        stdout, _ = child.communicate("resume\n", timeout=30)
        assert child.returncode == 0 and stdout.splitlines() == ["WARM_OLD_CAS_REFUSED", "COLD_OLD_CONSTRUCTOR_REFUSED"], stdout
        observed = subprocess.run(command + ["inspect", "catalog-legacy-probe"], cwd=root, check=True, capture_output=True, text=True, timeout=120)
        page = json.loads(observed.stdout)
        assert page["api_version"] == "playbooks-catalog-1" and page["items"] == []
        print(json.dumps({"passed": True, "old_archive_sha256": archive_hash, "old_version": "0.1.2", "new_version": "0.1.3", "warm_old_cas": "refused_by_database", "cold_old_constructor": "refused", "legacy_sequence": 99, "new_owner_catalog": page}, sort_keys=True))
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
