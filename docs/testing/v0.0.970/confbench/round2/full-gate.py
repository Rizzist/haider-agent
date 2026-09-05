import json,os,shutil,subprocess,time
from pathlib import Path
root=Path('/Users/rizzist/haider-run/lane-970-confbench')
out=root/'docs/testing/v0.0.970/confbench/round2/gate';out.mkdir(parents=True,exist_ok=True)
env=os.environ.copy()
env.update(RUST_MIN_STACK='8388608',HAIDER_DISCOVERY_DISABLED='1',HAIDER_TEST_DEVICE_NAME='test-mac',CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_BUILD_JOBS='2',CARGO_TARGET_DIR='/tmp/confbench-debug',RUST_TEST_THREADS='4')
steps=json.loads((out/'steps.json').read_text())
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

env['HAIDER_TEST_SIBLINGS_PREBUILT']='1'
assert Path('/tmp/confbench-debug/debug/haiderd').stat().st_size>10*1024*1024
run('workspace-test',['cargo','test','-q','--workspace','--no-fail-fast'])
run('clippy',['cargo','clippy','--workspace','--tests','--','-D','warnings'])
run('test-count-update',['cargo','run','-q','-p','xtask','--','test-count','--update'])
run('test-count',['cargo','run','-q','-p','xtask','--','test-count'])
run('fmt',['cargo','fmt','--all','--','--check'])
raise SystemExit(any(step['exit_code'] for step in steps))
