import os, subprocess, sys, time, json
from pathlib import Path
base=Path('/tmp/agentcli-round2')
env=dict(os.environ,RUST_MIN_STACK='8388608',HAIDER_DISCOVERY_DISABLED='1',HAIDER_TEST_DEVICE_NAME='test-mac',CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_BUILD_JOBS='2',TMPDIR='/tmp',RUST_TEST_THREADS='4')
name=sys.argv[1];cmd=sys.argv[2:]
if name not in ('siblings-build',):
 env['HAIDER_TEST_SIBLINGS_PREBUILT']='1'
with (base/f'{name}.disk').open('w') as disk:
 r=subprocess.run(['df','-m','/'],stdout=disk,text=True,check=True)
avail=int(subprocess.check_output(['df','-m','/'],text=True).splitlines()[-1].split()[3])
if avail<700: raise SystemExit('ENVIRONMENT-BLOCKED: disk below 700 MiB')
start=time.time()
with (base/f'{name}.log').open('w') as log:
 result=subprocess.run(cmd,env=env,stdout=log,stderr=subprocess.STDOUT)
record={'command':cmd,'env':{k:env[k] for k in ['RUST_MIN_STACK','HAIDER_DISCOVERY_DISABLED','HAIDER_TEST_DEVICE_NAME','CARGO_INCREMENTAL','CARGO_PROFILE_DEV_DEBUG','CARGO_BUILD_JOBS','TMPDIR','RUST_TEST_THREADS','HAIDER_TEST_SIBLINGS_PREBUILT'] if k in env},'exit':result.returncode,'elapsed_s':time.time()-start,'disk_mib_before':avail}
(base/f'{name}.json').write_text(json.dumps(record,indent=2));print(json.dumps(record),flush=True)
sys.exit(result.returncode)
