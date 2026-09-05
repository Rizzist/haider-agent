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

report_dir = Path('docs/testing/v0.0.970/agentcli-gate/repaint-diagnostic')
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
        trace = {'pid': self.pid, 'clean': result[0], 'audit': result[1], 'reap': owned}
        with (report_dir / 'pty-exit-trace.jsonl').open('a') as handle:
            handle.write(json.dumps(trace) + '\n')
        if not result[0]:
            (report_dir / f'failed-pty-{self.pid}.bin').write_bytes(self.sink[0])
            print('unclean PTY ' + json.dumps(trace), flush=True)
    return result
tui_probe.TuiProcess.close = observed_close

original_repaint_probelib = tui_probe._probelib
repaint_number = 0
def repaint_probelib():
    module = original_repaint_probelib()
    original_make = module.make_pump
    def make_pump(fd, sink):
        original_pump = original_make(fd, sink)
        started = time.monotonic()
        phase = 0
        mark = 0
        def pump(seconds):
            nonlocal phase, mark
            global repaint_number
            original_pump(seconds)
            if phase == 0 and time.monotonic() - started >= 1.0 and b'\x1b[?1049h' in sink[0] and b'message haider' not in module.plain(sink[0]):
                phase = 1
                module.set_size(fd, 118, 35)
            elif phase == 1:
                phase = 2
                mark = len(sink[0])
                module.set_size(fd, 118, 36)
            elif phase == 2:
                phase = 3
                repaint_number += 1
                frame = sink[0][mark:]
                rows = module.screen_rows(frame)
                visible = '\n'.join(rows.get(i, '') for i in range(1,37))
                record = {'index':repaint_number,'elapsed_s':time.monotonic()-started,'composer_visible':'message haider' in visible,'visible':visible}
                (report_dir / ('repaint-' + str(repaint_number) + '.json')).write_text(json.dumps(record, indent=2) + '\n')
                print('forced repaint',repaint_number,record['elapsed_s'],record['composer_visible'],flush=True)
        return pump
    module.make_pump = make_pump
    return module
tui_probe._probelib = repaint_probelib

from gate.contract import Evidence, PASS, FAIL
from dataclasses import replace
check = load_check(Path('scripts/qa-gate/checks/t0/t0.tui.palette_activation_closure.py'), 't0')
last_close = tui_probe.TuiProcess.close
current_command = 'seed'
def retained_close(self):
    result = last_close(self)
    (report_dir / (current_command + '-' + str(self.pid) + '.pty')).write_bytes(self.sink[0])
    return result
tui_probe.TuiProcess.close = retained_close
def run_history(ctx):
    global current_command
    status = tui_probe.start_daemon(ctx)
    rpc = tui_probe.RpcClient(status['daemon']['socket_path'])
    try:
        catalog = rpc.command_list('', in_session=True)
        session, _ = rpc.create_session(ctx.workspace_dir, provider='anthropic', model='claude-opus-5', effort='high', fast=False)
        check.module._pin_tui_identity(ctx)
        seed = tui_probe.TuiProcess(ctx, session_id=session)
        try:
            seed.type_slow('seed palette closure history')
            seed.enter()
            if not seed.wait_for(lambda raw: b'QA_PALETTE_HISTORY_READY' in raw):
                raise RuntimeError('history seed sentinel absent')
        finally:
            seed.close()
        evidence = []
        for command in ('fork', 'branch', 'undo', 'redo', 'checkpoints', 'rollback'):
            current_command = command
            try:
                result = check.module._activate(ctx, catalog, command, session)
                artifact = ctx.write_artefact(command + '-card.txt', result['text'])
                evidence.append(Evidence(command, PASS if result['pass'] else FAIL, result['line'], [artifact]))
                print(command, result['pass'], flush=True)
            except Exception as error:
                evidence.append(Evidence(command, FAIL, type(error).__name__ + ': ' + str(error)))
                print(command, str(error), flush=True)
        return evidence
    finally:
        rpc.close()
row, versions = runner.execute_check(replace(check, id='t0.tui.history_attach_diagnostic', run=run_history), bin_dir=bin_dir, measurement_accepted=False, report_artefact_root=report_dir / 'artefacts')
(report_dir / 'result.json').write_text(json.dumps({'binaries':metadata, 'diagnostic':True, 'row':row}, indent=2) + '\n')
print(row['status'], row['wall_ms'], flush=True)
