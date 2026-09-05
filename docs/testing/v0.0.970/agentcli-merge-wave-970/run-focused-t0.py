import json, pathlib, shutil, sys
root = pathlib.Path('/Users/rizzist/haider-run/lane-970-agentcli')
sys.path.insert(0, str(root / 'scripts/qa-gate'))
import runner
from gate.loader import load_check
from gate.report import binary_metadata
out=root/'docs/testing/v0.0.970/agentcli-merge-wave-970/t0'
out.mkdir(parents=True,exist_ok=True)
bins=pathlib.Path('/private/tmp/agentcli-merge-wave-970-bins')
bins.mkdir(exist_ok=True)
for name in ('haider','haiderd'):
    shutil.copy2(pathlib.Path('/private/tmp/haider-agentcli-target/debug')/name,bins/name)
metadata={n:binary_metadata(bins/n,n) for n in ('haider','haiderd')}
for name in metadata:
    metadata[name]['bytes']=(bins/name).stat().st_size
(out/'binaries.json').write_text(json.dumps(metadata,indent=2)+'\n')
failed=False
for name in ('t0.agent.spawn_result', 't0.tui.palette_activation_closure'):
    check=load_check(root/'scripts/qa-gate/checks/t0'/f'{name}.py','t0')
    if name.endswith('palette_activation_closure'):
        activate=check.module._activate
        def traced_activate(ctx,catalog,command,session_id):
            print('palette starting '+command,flush=True)
            row=activate(ctx,catalog,command,session_id)
            print('palette finished '+command+' '+str(row['pass']),flush=True)
            return row
        check.module._activate=traced_activate
    print('starting '+name,flush=True)
    row,versions=runner.execute_check(check,bin_dir=bins,measurement_accepted=False,report_artefact_root=out/'artefacts')
    (out/(name+'.row.json')).write_text(json.dumps(row,indent=2)+'\n')
    print(runner._render_check(row),flush=True)
    failed=failed or row['status']!='PASS'
raise SystemExit(int(failed))
