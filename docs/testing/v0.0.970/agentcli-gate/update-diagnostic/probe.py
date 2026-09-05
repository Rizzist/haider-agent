import argparse
import json
import os
from pathlib import Path
import sys
import time
sys.path.insert(0, str(Path.cwd() / 'scripts/qa-gate'))
import runner
from gate.loader import load_check
from gate import tui_probe
from gate.report import binary_metadata

report_dir = Path('docs/testing/v0.0.970/agentcli-gate/update-diagnostic')
report_dir.mkdir(parents=True, exist_ok=True)
bin_dir = Path('/private/tmp/agentcli-qa-final-bins')
metadata = {name: binary_metadata(bin_dir / name, name) for name in ('haider', 'haiderd')}
(report_dir / 'frozen-binaries.json').write_text(json.dumps(metadata, indent=2) + '\n')
print('frozen binaries ' + json.dumps(metadata), flush=True)

# Observe the existing reap law without changing any wait, kill, deadline, or verdict.
# A proxy scoped to each probelib instance records raw status and timeout SIGKILL.
original_probelib = tui_probe._probelib
reaps = []
class ObservedOS:
    def __init__(self, original):
        self.original = original
    def __getattr__(self, name):
        return getattr(self.original, name)
    def waitpid(self, pid, options):
        result = self.original.waitpid(pid, options)
        if result[0]:
            status = result[1]
            row = {'pid': pid, 'wait_options': options, 'raw_status': status,
                   'exit_code': self.original.WEXITSTATUS(status) if self.original.WIFEXITED(status) else None,
                   'signal': self.original.WTERMSIG(status) if self.original.WIFSIGNALED(status) else None}
            reaps.append(row)
        return result
    def kill(self, pid, signal):
        reaps.append({'pid': pid, 'kill_signal': int(signal)})
        return self.original.kill(pid, signal)
def observed_probelib():
    module = original_probelib()
    module.os = ObservedOS(module.os)
    return module
tui_probe._probelib = observed_probelib
original_close = tui_probe.TuiProcess.close
def observed_close(self):
    was_closed = self.closed
    result = original_close(self)
    if not was_closed:
        owned = [row for row in reaps if row['pid'] == self.pid]
        trace = {'pid': self.pid, 'clean': result[0], 'audit': result[1], 'reap': owned}
        with (report_dir / 'pty-exit-trace.jsonl').open('a') as handle:
            handle.write(json.dumps(trace) + '\n')
        if not result[0]:
            (report_dir / f'failed-pty-{self.pid}.bin').write_bytes(self.sink[0])
            print('unclean PTY ' + json.dumps(trace), flush=True)
    return result
tui_probe.TuiProcess.close = observed_close

from gate.context import CheckContext
from gate.contract import Evidence, PASS, FAIL
from dataclasses import replace
check = load_check(Path('scripts/qa-gate/checks/t0/t0.tui.palette_activation_closure.py'), 't0')
def run_update(ctx):
    status = tui_probe.start_daemon(ctx)
    rpc = tui_probe.RpcClient(status['daemon']['socket_path'])
    try:
        catalog = rpc.command_list('', in_session=True)
        session, _ = rpc.create_session(ctx.workspace_dir, provider='anthropic', model='claude-opus-5', effort='high', fast=False)
        result = check.module._activate(ctx, catalog, 'update', session)
        artifact = ctx.write_artefact('update-card.txt', result['text'])
        return [Evidence('update', PASS if result['pass'] else FAIL, result['line'], [artifact])]
    finally:
        rpc.close()
row, versions = runner.execute_check(replace(check, id='t0.tui.update_exit_diagnostic', run=run_update), bin_dir=bin_dir, measurement_accepted=False, report_artefact_root=report_dir / 'artefacts')
(report_dir / 'result.json').write_text(json.dumps({'binaries':metadata, 'diagnostic':True, 'row':row}, indent=2) + '\n')
print(json.dumps(row), flush=True)
