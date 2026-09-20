#!/usr/bin/env python3
"""Exercise the parity review surfaces through a real PTY.

Runs the demo twice so both typed permission decisions are observed. The
reject run also navigates to the second hunk and opens the read-only inbox.
"""

import os
import signal
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib


def wait_for(pump, sink, needle, timeout=12.0, start=0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        pump(0.1)
        if needle.encode() in probelib.plain(sink[0][start:]):
            return True
    return False


def command(fd, pump, text):
    os.write(fd, text.encode())
    pump(0.3)
    os.write(fd, b"\x1b")
    pump(0.2)
    os.write(fd, b"\r")


def strict_reap(pid, timeout=3.0):
    """Require a clean exit without blocking forever on a failed child."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            done, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return False
        if done:
            return os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0
        time.sleep(0.05)
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return False
    # A failed probe still has to release its lane-owned child. A bounded
    # nonblocking reap avoids the macOS PTY waitpid stall seen after SIGKILL.
    deadline = time.time() + 1.0
    while time.time() < deadline:
        try:
            done, _ = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return False
        if done:
            return False
        time.sleep(0.05)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    return False


def run(binary, decision):
    pid, fd = probelib.spawn(118, 36, binary)
    sink = [b""]
    pump = probelib.make_pump(fd, sink)
    checks = []
    launcher = wait_for(pump, sink, "start a session")
    checks.append(("launcher_ready", launcher))
    if launcher:
        os.write(fd, b"review diff before applying it\r")
    checks.append(("review_open", wait_for(pump, sink, "EDIT REVIEW")))
    checks.append(("first_hunk", wait_for(pump, sink, "hunk 1/2")))
    if decision == "reject":
        os.write(fd, b"n")
        pump(0.2)
        probelib.set_size(fd, 117, 36)
        os.kill(pid, signal.SIGWINCH)
        pump(0.2)
        probelib.set_size(fd, 118, 36)
        os.kill(pid, signal.SIGWINCH)
        checks.append(("second_hunk", wait_for(pump, sink, "hunk 2/2")))
        os.write(fd, b"2")
        checks.append(("reject_echo", wait_for(pump, sink, "permission rejected")))
        # Slash palette opens on `/`; Esc dismisses it and Enter executes the
        # exact command, matching the interactive command path.
        command(fd, pump, "/inbox")
        checks.append(("inbox_surface", wait_for(pump, sink, "NEEDS YOU")))
        os.write(fd, b"\x1b")
        pump(0.3)
    else:
        os.write(fd, b"1")
        checks.append(("allow_echo", wait_for(pump, sink, "permission allowed once")))

    # Ctrl-C walks session → launcher, then quits from the launcher. Wait
    # for the intermediate repaint so the second key cannot race the first.
    launcher_offset = len(sink[0])
    os.write(fd, b"\x03")
    returned_to_launcher = wait_for(
        pump, sink, "recent sessions", timeout=5.0, start=launcher_offset
    )
    checks.append(("returned_to_launcher", returned_to_launcher))
    os.write(fd, b"\x03")
    probelib.drain_quiet(fd, sink)
    clean = strict_reap(pid)
    return sink[0], clean, checks


binary = sys.argv[1] if len(sys.argv) > 1 else "/usr/local/bin/haider"
choices = sys.argv[2:] or ("allow", "reject")
all_output = b""
all_checks = []
all_clean = True
for choice in choices:
    output, clean, checks = run(binary, choice)
    all_output += output
    all_clean = all_clean and clean
    all_checks.extend((f"{choice}_{name}", passed) for name, passed in checks)

if any(not passed for _, passed in all_checks):
    print(probelib.plain(all_output)[-4000:].decode("utf-8", "replace"))
probelib.verdict("PTY_PARITY_REVIEW", all_output, all_clean, all_checks)
