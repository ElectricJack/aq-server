#!/usr/bin/env python3
"""Independent canonical hashes only: no owner truth or live permission."""
import argparse, hashlib, json
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
def canonical(v):
    return json.dumps(v, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
def digest(domain, v):
    return hashlib.sha256(canonical([domain, v])).hexdigest()
def vectors():
    f = json.loads((ROOT / "artifacts/preparation-v2.fixture.json").read_text())
    contract = json.loads((ROOT / "artifacts/preparation-v2.contract.json").read_text())
    schema = json.loads((ROOT / "artifacts/preparation-v2.schema.json").read_text())
    out = {"schema": hashlib.sha256(canonical(schema)).hexdigest(), "contract": digest("aq.phase.contract.v2", contract["manifest"])}
    for key, domain in [("proposal", "proposal"), ("accept", "accept"), ("accepted", "accepted-receipt"), ("prepare", "prepare"), ("execution", "execution-receipt")]:
        out[key] = digest("aq.phase." + domain + ".v2", f[key])
    out["confirmation"] = digest("aq.phase.confirmation.v2", f["execution"]["confirmation"])
    out["facts"] = [digest("aq.phase.owner-fact.v2", x["body"]) for x in f["execution"]["confirmation"]["facts"]]
    unicode = f["execution"]["confirmation"]["facts"][0]["body"].copy()
    unicode["authority"] = dict(unicode["authority"], id="synthetic-owner/雪")
    out["unicode_fact"] = digest("aq.phase.owner-fact.v2", unicode)
    assert out["contract"] == contract["artifact"]["digest"]
    assert out["schema"] == contract["manifest"]["schema_digest"]
    return out
if __name__ == "__main__":
    p = argparse.ArgumentParser(); p.add_argument("--check", action="store_true"); args = p.parse_args()
    path = ROOT / "artifacts/preparation-v2.python-vectors.json"
    value = vectors()
    if args.check:
        assert value == json.loads(path.read_text()), "phase canonical vectors drift"
        print("phase canonical vectors match")
    else:
        path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
