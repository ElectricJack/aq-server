from pathlib import Path
import tarfile, subprocess, os, hashlib, json, sys
evidence=Path(__file__).resolve().parent
source=evidence.parents[2]
consumer=Path(sys.argv[1]).resolve()
target=Path(sys.argv[2]).resolve()
consumer.mkdir(exist_ok=False)
archives=[evidence/'aq-phase-terminal-protocol-0.1.0.crate']
archives+=sorted((source/'docs/implementation/AQ-08-admission019-evidence').glob('*.crate'))
expected=json.loads((evidence/'archive-manifest.json').read_text())
manifest={}
for p in archives:
    assert hashlib.sha256(p.read_bytes()).hexdigest()==expected[p.name]['sha256']
    manifest[p.name]={'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'bytes':p.stat().st_size}
    with tarfile.open(p) as t:
        for member in t.getmembers():
            assert not Path(member.name).is_absolute() and '..' not in Path(member.name).parts and (member.isdir() or member.isfile())
        t.extractall(consumer,filter='data')
(consumer/'Cargo.toml').write_text('''[workspace]
resolver = "3"
members = ["aq-phase-terminal-protocol-0.1.0"]
[patch.crates-io]
aq-admission = { path = "aq-admission-0.1.9" }
aq-session-protocol = { path = "aq-session-protocol-0.1.0" }
aq-lifecycle-protocol = { path = "aq-lifecycle-protocol-0.1.0" }
''')
(consumer/'archive-manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
env=dict(os.environ,CARGO_TARGET_DIR=str(target),CARGO_BUILD_JOBS='2',CARGO_PROFILE_DEV_DEBUG='0',CARGO_INCREMENTAL='0')
commands=[('lock',['cargo','generate-lockfile','--offline']),('tests',['cargo','test','-p','aq-phase-terminal-protocol','--all-features','--locked','--offline']),('clippy',['cargo','clippy','-p','aq-phase-terminal-protocol','--all-targets','--all-features','--locked','--offline','--','-D','warnings']),('format',['cargo','fmt','-p','aq-phase-terminal-protocol','--','--check']),('schema',['python3','aq-phase-terminal-protocol-0.1.0/scripts/validate_schema.py'])]
for name,cmd in commands:
    with open(consumer/('qualification-'+name+'.log'),'w') as log:
        subprocess.run(cmd,cwd=consumer,env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
print('PASS exact extracted archive/dependency qualification')
