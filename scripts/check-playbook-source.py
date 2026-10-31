#!/usr/bin/env python3
"""Check the exact vendored source closure without any sibling checkout/network."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
pin = json.loads((ROOT / "vendor/aq-playbooks-source.json").read_text())
base = ROOT / pin["vendor_root"]
expected = {entry["path"]: entry for entry in pin["files"]}
actual = {str(path.relative_to(base)) for path in base.rglob("*") if path.is_file()}
assert actual == set(expected), f"source file set drift: {actual ^ set(expected)}"
for name, entry in expected.items():
    path = base / name
    assert not path.is_symlink(), f"unexpected symlink: {name}"
    assert bool(path.stat().st_mode & 0o111) == (entry["mode"] == "100755"), f"mode drift: {name}"
    data = path.read_bytes()
    assert len(data) == entry["bytes"], f"size drift: {name}"
    assert hashlib.sha256(data).hexdigest() == entry["sha256"], f"hash drift: {name}"
print(f"exact aq-playbooks {pin['source_revision']}: {len(expected)} files verified")
