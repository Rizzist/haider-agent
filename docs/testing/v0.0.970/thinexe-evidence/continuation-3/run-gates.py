import json, os, subprocess, sys, time
from pathlib import Path
root=Path.cwd(); out=root/'docs/testing/v0.0.970/thinexe-evidence/continuation-3'
env=dict(os.environ,RUST_MIN_STACK='8388608',HAIDER_DISCOVERY_DISABLED='1',HAIDER_TEST_DEVICE_NAME='test-mac',CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',HAIDER_TEST_SIBLINGS_PREBUILT='1',CARGO_BUILD_JOBS='2',CARGO_TARGET_DIR='/private/tmp/haider-thinexe-target')
commands=[
 ('build-siblings',['cargo','build','-q','-p','haider-cli','-p','haider-daemond','-p','haider-tui-exe','-p','haider-tools','--bins']),
 ('goldens',['cargo','test','-q','-p','haider-cli','--test','turnhygiene_pin_tests','golden']),
 ('instruct-pin',['cargo','test','-q','-p','haider-daemon','--lib','instruct_pipe_shrinks_the_advertised_wire_pack','--','--nocapture']),
 ('test-count',['cargo','run','-q','-p','xtask','--','test-count','--update']),
 ('workspace-tests',['cargo','test','-q','--workspace','--no-fail-fast']),
 ('clippy',['cargo','clippy','--workspace','--tests','--','-D','warnings']),
]
if sys.argv[1:] in (['quiet-final'], ['limited-final']):
 commands=commands[-2:]
 out=out/sys.argv[1]; out.mkdir(exist_ok=True)
 if sys.argv[1]=='limited-final': env['RUST_TEST_THREADS']='2'
rows=[]
for name,cmd in commands:
 disk=subprocess.check_output(['df','-m','/'],text=True)
 if int(disk.splitlines()[1].split()[3])<700: raise SystemExit('ENVIRONMENT-BLOCKED: less than 700 MiB free')
 command_env=dict(env)
 if name=='goldens': command_env['UPDATE_FIXTURES']='1'
 start=time.monotonic(); print('START',name,flush=True)
 with (out/(name+'.log')).open('w') as log:
  log.write(disk); log.flush()
  result=subprocess.run(cmd,env=command_env,stdout=log,stderr=subprocess.STDOUT)
 row=dict(name=name,argv=cmd,elapsed_seconds=time.monotonic()-start,exit_code=result.returncode,env={k:command_env[k] for k in env if k.startswith(('CARGO_','HAIDER_','RUST_')) and k in ['CARGO_TARGET_DIR','CARGO_BUILD_JOBS','CARGO_INCREMENTAL','CARGO_PROFILE_DEV_DEBUG','RUST_MIN_STACK','HAIDER_DISCOVERY_DISABLED','HAIDER_TEST_DEVICE_NAME','HAIDER_TEST_SIBLINGS_PREBUILT','RUST_TEST_THREADS']})
 rows.append(row); (out/'gates.json').write_text(json.dumps(rows,indent=2)+'\n'); print('END',row,flush=True)
 if result.returncode: raise SystemExit(result.returncode)
 if name=='build-siblings':
  size=Path(env['CARGO_TARGET_DIR']+'/debug/haiderd').stat().st_size
  assert size>10*1024*1024,size
  (out/'daemon-size.txt').write_text(str(size)+'\n')
