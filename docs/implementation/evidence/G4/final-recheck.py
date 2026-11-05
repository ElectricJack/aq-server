import os, subprocess
from pathlib import Path

def test_recheck():
    env=os.environ.copy()
    env.update(CARGO_BUILD_JOBS='2',RUST_TEST_THREADS='2',DATABASE_URL='postgres://postgres:test@127.0.0.1:55432/postgres')
    commands=[
      ('final-search-fixed',['cargo','test','--locked','-p','aq-server','--test','dashboard_search']),
      ('final-fmt-after-fix',['cargo','fmt','--check']),
      ('final-clippy-after-fix',['cargo','clippy','--locked','--all-targets','--','-D','warnings']),
    ]
    failures=[]
    for name,cmd in commands:
        p=subprocess.run(cmd,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        Path('docs/implementation/evidence/G4/'+name+'.log').write_text(p.stdout)
        if p.returncode: failures.append((name,p.returncode))
    assert not failures, str(failures)
