"""macOS-only, task-owned load and child-descheduling fault injection.

Use --binary with the original or fixed haider-platform libtest executable.
--hold-ms=100 models a child descheduled for twice the old 50ms deadline;
this delay belongs only to the fault injector, never the product/test fix.
"""
import argparse
import concurrent.futures
import ctypes
import json
import os
from pathlib import Path
import signal
import subprocess
import time

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--iterations', type=int, default=10)
parser.add_argument('--hogs', type=int, default=32)
parser.add_argument('--workers', type=int, default=1)
parser.add_argument('--hold-ms', type=int, default=0)
parser.add_argument('--suite', action='store_true')
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
lib = ctypes.CDLL('/usr/lib/libproc.dylib')
lib.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
command = [str(Path(args.binary).resolve())]
command += ['--test-threads=4'] if args.suite else [
    '--exact', 'process::tests::armed_kqueue_observes_a_short_command_without_coarse_backoff',
    '--nocapture',
]
env = dict(os.environ, RUST_MIN_STACK='8388608', HAIDER_DISCOVERY_DISABLED='1',
           HAIDER_TEST_DEVICE_NAME='test-mac', CARGO_INCREMENTAL='0', CARGO_PROFILE_DEV_DEBUG='0')


def probe(index):
    started = time.monotonic()
    process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True)
    child = None
    stopped = False
    try:
        if args.hold_ms:
            while process.poll() is None and time.monotonic() - started < 1:
                pids = (ctypes.c_int * 32)()
                count = lib.proc_listchildpids(process.pid, pids, ctypes.sizeof(pids))
                if count > 0 and pids[0] > 0:
                    child = pids[0]
                    try:
                        os.kill(child, signal.SIGSTOP)
                        stopped = True
                    except ProcessLookupError:
                        child = None
                    break
            if child is not None:
                time.sleep(args.hold_ms / 1000)
                try:
                    os.kill(child, signal.SIGCONT)
                except ProcessLookupError:
                    pass
                stopped = False
        output = process.communicate(timeout=60)[0]
    finally:
        if stopped:
            try:
                os.kill(child, signal.SIGCONT)
            except ProcessLookupError:
                pass
        if process.poll() is None:
            process.kill()
            process.wait()
    (args.output / f'probe-{index + 1}.log').write_text(output)
    return {'iteration': index + 1, 'exit': process.returncode,
            'held_child': child, 'hold_ms': args.hold_ms,
            'elapsed_ms': round((time.monotonic() - started) * 1000, 2)}


hogs = []
try:
    for _ in range(args.hogs):
        hogs.append(subprocess.Popen(['/usr/bin/yes'], stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL))
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        rows = list(pool.map(probe, range(args.iterations)))
finally:
    for hog in hogs:
        hog.terminate()
    for hog in hogs:
        hog.wait()
(args.output / 'results.json').write_text(json.dumps({'hogs': args.hogs,
    'workers': args.workers, 'command': command, 'results': rows}, indent=2))
print(json.dumps({'runs': len(rows), 'failures': sum(r['exit'] != 0 for r in rows),
                  'held': sum(r['held_child'] is not None for r in rows)}))
