import os, shlex, runpy
from pathlib import Path
for line in Path('/tmp/agentcli-round2/xwin-env.txt').read_text().splitlines():
 parts=shlex.split(line.rstrip(';'))
 assert len(parts)==2 and parts[0]=='export', parts
 key,value=parts[1].split('=',1)
 os.environ[key]=value
os.environ['CARGO_HOME']='/tmp/agentcli-round2/cargo-home'
os.environ['CARGO_NET_OFFLINE']='true'
os.environ['CARGO_TARGET_DIR']='/tmp/agentcli-round2/windows-target'
runpy.run_path('/tmp/agentcli-round2/run_gate.py',run_name='__main__')
