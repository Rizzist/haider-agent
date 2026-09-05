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

report_dir = Path('docs/testing/v0.0.970/agentcli-gate/final-fixed')
report_dir.mkdir(parents=True, exist_ok=True)
bin_dir = Path('/private/tmp/agentcli-qa-postupdate-bins')
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
        trace = {'pid': self.pid, 'command': globals().get('current_palette_command', 'seed'), 'clean': result[0], 'audit': result[1], 'reap': owned}
        with (report_dir / 'pty-exit-trace.jsonl').open('a') as handle:
            handle.write(json.dumps(trace) + '\n')
        (report_dir / f'pty-{self.pid}.bin').write_bytes(self.sink[0])
        if not result[0]:
            print('unclean PTY ' + json.dumps(trace), flush=True)
    return result
tui_probe.TuiProcess.close = observed_close
checks = [load_check(Path('scripts/qa-gate/checks/t0') / filename, 't0') for filename in (
    't0.agent.spawn_result.py', 't0.tui.palette_activation_closure.py')]
palette = checks[1].module
original_activate = palette._activate
def logged_activate(ctx, catalog, command, session_id):
    global current_palette_command
    current_palette_command = command
    print('palette starting ' + command, flush=True)
    result = original_activate(ctx, catalog, command, session_id)
    print('palette finished ' + command + ' ' + str(result['pass']), flush=True)
    return result
palette._activate = logged_activate
runner.discover_checks = lambda root, tier: checks
original_execute = runner.execute_check
def observed_execute(check, **kwargs):
    print('starting ' + check.id, flush=True)
    row, versions = original_execute(check, **kwargs)
    (report_dir / (check.id + '.row.json')).write_text(json.dumps(row, indent=2) + '\n')
    print('finished ' + check.id + ' ' + row['status'] + ' ' + str(row['wall_ms']) + 'ms', flush=True)
    return row, versions
runner.execute_check = observed_execute
raise SystemExit(runner.run_tier(argparse.Namespace(tier='t0', bin_dir=bin_dir, report_dir=report_dir)))
