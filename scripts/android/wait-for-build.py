#!/usr/bin/env python3
"""Bounded shared-Mac build admission. Does not stop anyone else's processes."""
import argparse
import os
import re
import subprocess
import sys
import time


def ancestors():
    result = set()
    pid = os.getppid()
    while pid > 1 and pid not in result:
        result.add(pid)
        pid = int(subprocess.check_output(['ps', '-o', 'ppid=', '-p', str(pid)], text=True).strip() or '0')
    return result


def ready(ignore_ancestors=False):
    load = os.getloadavg()[0]
    vm = subprocess.check_output(['vm_stat'], text=True)
    page = int(re.search(r'page size of (\d+) bytes', vm)[1])
    pages = sum(int(re.search(rf'Pages {kind}:\s+(\d+)', vm)[1]) for kind in ('free', 'inactive'))
    processes = subprocess.check_output(['ps', '-axo', 'pid=,comm=,args='], text=True)
    ignored = ancestors() if ignore_ancestors else set()
    gradle = False
    for line in processes.splitlines():
        fields = line.strip().split(None, 2)
        if len(fields) == 3 and int(fields[0]) not in ignored:
            executable = fields[1].rsplit('/', 1)[-1]
            if executable == 'gradle' or (executable == 'java' and re.search('gradle', fields[2], re.I)):
                gradle = True
    memory = pages * page / 1024**3
    print(f'load1={load:.2f} free+inactiveGiB={memory:.2f} gradle={gradle}', flush=True)
    return load < 6 and memory > 3 and not gradle


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--ignore-ancestors', action='store_true', help='For a native build inside the already admitted owning Gradle only')
    args = parser.parse_args()
    if sys.platform != 'darwin':
        return 0  # Hosted CI owns its isolated runner.
    deadline = time.monotonic() + 1800
    while True:
        if ready(args.ignore_ancestors):
            return 0
        if time.monotonic() >= deadline:
            print('Build admission timed out after 30 minutes', file=sys.stderr)
            return 75
        time.sleep(min(60, max(0, deadline - time.monotonic())))


if __name__ == '__main__':
    sys.exit(main())
