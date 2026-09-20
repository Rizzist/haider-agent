#!/usr/bin/env python3
"""Exercise the 972 band-report surfaces through a real PTY (demo mode).

Walks /providers and /usage and asserts each report renders with its key
map riding the shared bottom band's slot (the footer redesign), then
checks /sessions keeps its honest demo refusal (the browser is daemon
truth, live only). Esc walks each report back to the launcher.
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


def run(binary):
    pid, fd = probelib.spawn(118, 36, binary)
    sink = [b""]
    pump = probelib.make_pump(fd, sink)
    checks = []
    checks.append(("launcher_ready", wait_for(pump, sink, "start a session")))

    # /providers — the registry report with the key map in the band slot.
    command(fd, pump, "/providers")
    checks.append(("providers_open", wait_for(pump, sink, "PROVIDERS")))
    checks.append(
        ("providers_band_map", wait_for(pump, sink, "model click sets default"))
    )
    checks.append(("providers_way_out", wait_for(pump, sink, "esc back")))
    offset = len(sink[0])
    os.write(fd, b"\x1b")
    checks.append(
        ("providers_esc_back", wait_for(pump, sink, "start a session", start=offset))
    )

    # /usage — the honest demo state, key map in the slot.
    command(fd, pump, "/usage")
    checks.append(("usage_open", wait_for(pump, sink, "USAGE")))
    checks.append(
        (
            "usage_demo_honesty",
            wait_for(pump, sink, "usage is live daemon truth"),
        )
    )
    checks.append(("usage_band_map", wait_for(pump, sink, "f refresh")))
    offset = len(sink[0])
    os.write(fd, b"\x1b")
    checks.append(
        ("usage_esc_back", wait_for(pump, sink, "start a session", start=offset))
    )

    # /sessions — the demo keeps its honest daemon-truth refusal.
    offset = len(sink[0])
    command(fd, pump, "/sessions")
    checks.append(
        ("sessions_demo_refusal", wait_for(pump, sink, "live only", start=offset))
    )

    # Quit from the launcher.
    os.write(fd, b"\x03")
    probelib.drain_quiet(fd, sink)
    clean = strict_reap(pid)
    return sink[0], clean, checks


binary = sys.argv[1] if len(sys.argv) > 1 else "/usr/local/bin/haider"
output, clean, checks = run(binary)
if any(not passed for _, passed in checks):
    print(probelib.plain(output)[-4000:].decode("utf-8", "replace"))
probelib.verdict("PTY_BAND_REPORTS", output, clean, checks)
