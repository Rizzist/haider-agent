import json, os, pathlib, subprocess, sys, time
root = pathlib.Path('/Users/rizzist/haider-run/lane-970-agentcli')
out = root / 'docs/testing/v0.0.970/agentcli-merge-wave-970'
out.mkdir(exist_ok=True)
name, *command = sys.argv[1:]
env = dict(os.environ, RUST_MIN_STACK='8388608', HAIDER_DISCOVERY_DISABLED='1',
    HAIDER_TEST_DEVICE_NAME='test-mac', CARGO_INCREMENTAL='0', CARGO_PROFILE_DEV_DEBUG='0',
    HAIDER_TEST_SIBLINGS_PREBUILT='1', CARGO_BUILD_JOBS='4',
    CARGO_TARGET_DIR='/private/tmp/haider-agentcli-target', TMPDIR='/tmp', RUST_TEST_THREADS='4')
disk = subprocess.check_output(['df', '-m', '/'], text=True)
(out / (name + '.disk')).write_text(disk)
available = int(disk.splitlines()[-1].split()[3])
if available < 700:
    print('ENVIRONMENT-BLOCKED: disk under 700 MiB', flush=True)
    sys.exit(78)
record = {'command':command, 'environment':{k:env[k] for k in ('RUST_MIN_STACK','HAIDER_DISCOVERY_DISABLED','HAIDER_TEST_DEVICE_NAME','CARGO_INCREMENTAL','CARGO_PROFILE_DEV_DEBUG','HAIDER_TEST_SIBLINGS_PREBUILT','CARGO_BUILD_JOBS','CARGO_TARGET_DIR','TMPDIR','RUST_TEST_THREADS')}, 'disk_available_mib':available}
print(f'{name}: started; disk={available} MiB; command={command}', flush=True)
start=time.monotonic()
with (out / (name + '.log')).open('w') as log:
    result = subprocess.run(command, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
record.update(exit=result.returncode, elapsed_s=round(time.monotonic()-start,3))
(out/(name+'.json')).write_text(json.dumps(record,indent=2)+'\n')
print(f'{name}: exit={result.returncode}, elapsed={record["elapsed_s"]}s', flush=True)
print('\n'.join((out/(name+'.log')).read_text(errors='replace').splitlines()[-16:]), flush=True)
sys.exit(result.returncode)
