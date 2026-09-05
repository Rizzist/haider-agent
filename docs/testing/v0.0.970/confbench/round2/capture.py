import sys, json, dataclasses, hashlib
from pathlib import Path
sys.path.insert(0,'/Users/rizzist/Documents/CODING/haidercode-web')
from bench.adapters import load_adapter
from bench.conformance.runner import _run_once
adapter=load_adapter('haider-agent')
out=Path(sys.argv[2]);out.mkdir(parents=True,exist_ok=True)
for scenario in (sys.argv[3:] or ['malformed_tool','auxiliary_probe']):
 e=_run_once(adapter,scenario,'deepseek-v4-flash',sys.argv[1],15,20,131072,8192,20)
 (out/(scenario+'.jsonl')).write_text(e.stdout_text)
 (out/(scenario+'.stderr')).write_text(e.stderr_text)
 (out/(scenario+'.normalized.json')).write_text(json.dumps(e.normalized_events,indent=2))
 (out/(scenario+'.evidence.json')).write_text(json.dumps(dict(scenario=scenario,exit_code=e.exit_code,timed_out=e.timed_out,requests=[dict(path=x.path,model=x.model) for x in e.requests],auxiliary_tool_name=e.auxiliary_tool_name,proxy_tool_call_ids=e.proxy_tool_call_ids,normalized_events=e.normalized_events,jsonl_sha256=hashlib.sha256(e.stdout_text.encode()).hexdigest()),indent=2)+'\n')
 print(scenario, 'exit',e.exit_code,'requests',len(e.requests),'auxiliary',e.auxiliary_tool_name,flush=True)
 print([(x.path,x.model) for x in e.requests],flush=True)
