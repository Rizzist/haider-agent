import json, sys, time
from pathlib import Path
root=Path.cwd()
sys.path.insert(0,str(root/'scripts/qa-gate'))
from gate.loader import load_check
from runner import execute_check
out=Path(sys.argv[1]).resolve(); out.mkdir(parents=True,exist_ok=True)
ids=sys.argv[2:] or ['t1.install.paths','t1.store.previous_release_upgrade']
rows=[]
for name in ids:
 check=load_check(root/'scripts/qa-gate/checks/t1'/f'{name}.py')
 start=time.monotonic()
 row,_=execute_check(check,bin_dir=Path('/private/tmp/haider-thinexe-target/debug'),measurement_accepted=False,report_artefact_root=out)
 row['measured_elapsed_seconds']=time.monotonic()-start
 rows.append(row)
 (out/'checks.json').write_text(json.dumps(rows,indent=2)+'\n')
 print(name,row['status'],row['measured_elapsed_seconds'],flush=True)
 for evidence in row['evidence']: print(evidence,flush=True)
raise SystemExit(any(row['status']!='PASS' for row in rows))
