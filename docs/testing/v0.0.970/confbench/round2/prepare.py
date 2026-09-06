import json,os,shutil,subprocess,time
from pathlib import Path
root=Path('/Users/rizzist/haider-run/lane-970-confbench')
out=root/'docs/testing/v0.0.970/confbench/round2/gate';out.mkdir(parents=True,exist_ok=True)
env=os.environ.copy()
env.update(RUST_MIN_STACK='8388608',HAIDER_DISCOVERY_DISABLED='1',HAIDER_TEST_DEVICE_NAME='test-mac',CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_BUILD_JOBS='2',CARGO_TARGET_DIR='/tmp/confbench-debug',RUST_TEST_THREADS='4')
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
if run('siblings',['cargo','build','-p','haider-cli','-p','haider-daemond']):
    raise SystemExit('sibling prebuild failed')
size=Path('/tmp/confbench-debug/debug/haiderd').stat().st_size
(out/'sibling-size.txt').write_text(str(size)+'\n')
assert size>10*1024*1024,size
env['HAIDER_TEST_SIBLINGS_PREBUILT']='1'
run('protocol',['cargo','test','-q','-p','haider-protocol','--test','golden_tests','invalid_tool_call'])
run('malformed',['cargo','test','-q','-p','haider-core','--test','runtime_tests','malformed'])
run('core-exposure',['cargo','test','-q','-p','haider-core','--lib','tool_exposure'])
run('pipe-pin',['cargo','test','-q','-p','haider-daemon','--lib','instruct_pipe_shrinks_the_advertised_wire_pack','--','--nocapture'])
run('jsonl-replay',['cargo','test','-q','-p','haider-cli','--test','cli_tests','malformed_attempt_stays_failed_after_successful_repair_in_jsonl_and_replay'])
run('golden',['cargo','test','-q','-p','haider-cli','--test','turnhygiene_pin_tests','provider_request_body_is_budget_independent_and_matches_the_golden_ledger'],{'UPDATE_FIXTURES':'1'})
run('baseline-before-gate',['cargo','run','-q','-p','xtask','--','test-count','--update'])

raise SystemExit(any(step["exit_code"] for step in steps))
