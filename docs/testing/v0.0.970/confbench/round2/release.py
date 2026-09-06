import json,os,shutil,subprocess,time
from pathlib import Path
root=Path('/Users/rizzist/haider-run/lane-970-confbench')
out=root/'docs/testing/v0.0.970/confbench/round2/release';out.mkdir(parents=True,exist_ok=True)
env=os.environ.copy()
env.update(RUST_MIN_STACK='8388608',HAIDER_DISCOVERY_DISABLED='1',HAIDER_TEST_DEVICE_NAME='test-mac',CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_BUILD_JOBS='2',CARGO_TARGET_DIR=str(root/'target'),RUST_TEST_THREADS='4')
steps=[]
def run(name,args,extra=None):
    disk=subprocess.run(['df','-m','/'],text=True,capture_output=True,check=True).stdout
    (out/(name+'.disk')).write_text(disk)
    if int(disk.splitlines()[1].split()[3])<700:
        raise SystemExit('ENVIRONMENT-BLOCKED: free disk under 700 MiB')
    started=time.time();print('START',name,flush=True)
    with (out/(name+'.log')).open('w') as log:
        result=subprocess.run(args,cwd=root,env=env|dict(extra or {}),stdout=log,stderr=subprocess.STDOUT)
    steps.append(dict(name=name,args=args,exit_code=result.returncode,seconds=round(time.time()-started,2)))
    (out/'steps.json').write_text(json.dumps(steps,indent=2)+'\n')
    print('END',name,result.returncode,flush=True)
    return result.returncode

if run('build',['cargo','build','--release','-p','haider-cli','-p','haider-daemond']):
    raise SystemExit('release build failed')
import hashlib
from datetime import datetime,timezone
artifacts={}
for name in ['haider','haiderd']:
    binary=root/'target/release'/name
    version_home=root/'tmp/confbench/round2-version-home'
    version_home.mkdir(parents=True,exist_ok=True)
    version_profile=version_home/'profile'
    version_profile.mkdir(exist_ok=True)
    r=subprocess.run([str(binary),'--version'],env=env|{'HOME':str(version_home),'HAIDER_PROFILE_DIR':str(version_profile)},text=True,capture_output=True,check=True,timeout=30)
    artifacts[name]={'bytes':binary.stat().st_size,'sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'version':r.stdout.strip()}
    assert binary.stat().st_size>10*1024*1024
(out/'artifacts.json').write_text(json.dumps({'recorded_at':datetime.now(timezone.utc).isoformat(),'merged_upstream':subprocess.check_output(['git','--git-dir=/tmp/confbench-lane.git','rev-parse','origin/wave-970'],text=True).strip(),'source':'round2 lane diff atop 72364f34; final bundle commit identifies tested source','executables':artifacts},indent=2)+'\n')
peer=Path('/Users/rizzist/Documents/CODING/haidercode-web')
env['PYTHONDONTWRITEBYTECODE']='1'
bench_out=out.parent
args=['python3','-m','bench.conformance','--adapter','haider-agent','--executable',str(root/'target/release/haider'),'--model','deepseek-v4-flash','--context-window','131072','--max-output-tokens','8192','--max-turns','20','--process-timeout','15','--proxy-timeout','20','--json-report',str(bench_out/'haider-970-round2.json')]
print('START old-bench',flush=True)
with (bench_out/'benchmark.log').open('w') as log:
    r=subprocess.run(args,cwd=peer,env=env,stdout=log,stderr=subprocess.STDOUT)
(bench_out/'benchmark-command.json').write_text(json.dumps({'args':args,'cwd':str(peer),'exit_code':r.returncode},indent=2)+'\n')
print('END old-bench',r.returncode,flush=True)
with (bench_out/'diagnostic.log').open('w') as log:
    r=subprocess.run(['python3','/tmp/confbench-capture.py',str(root/'target/release/haider'),str(bench_out/'diagnostic')],cwd=peer,env=env,stdout=log,stderr=subprocess.STDOUT)
assert r.returncode==0
hash_path=bench_out/'peer-file-sha256.json'
proof=json.loads(hash_path.read_text())
proof['after']={p:hashlib.sha256((peer/p).read_bytes()).hexdigest() for p in proof['before']}
proof['unchanged']=proof['before']==proof['after']
hash_path.write_text(json.dumps(proof,indent=2)+'\n')
assert proof['unchanged']
print('END diagnostic and peer hashes',flush=True)
