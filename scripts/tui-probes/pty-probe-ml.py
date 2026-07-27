#!/usr/bin/env python3
"""pty-probe variant that DRIVES input: start a session ("hi" + Enter),
then build a 3-line composer via Alt+Enter (ESC CR). Keystrokes are PACED
(real terminals emit Alt+Enter atomically; unpaced bursts can split at the
kernel read boundary and race ESC disambiguation), and the capture ends
with a resize-forced FULL repaint + silence drain so the check reads the
final screen, not scheduler luck. GATES (review TUI4.1 P2-3): the three
composer lines on the final screen, alt-screen balance, no panic, clean
child exit on the ⌃C ⌃C quit path."""
import os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

pid, fd = probelib.spawn(cols, rows, binary)
sink = [b""]
pump = probelib.make_pump(fd, sink)

pump(4.5)  # boot -> launcher (cold-build tolerant)
os.write(fd, b"hi\r")  # start a session
pump(1.5)
for part in (b"alpha", b"\x1b\r", b"beta", b"\x1b\r", b"gamma"):
    os.write(fd, part)  # Alt+Enter = ESC CR, delivered atomically
    pump(0.12)
pump(1.0)
# Force a FULL final repaint so the check reads the final screen.
pre = len(sink[0])
probelib.set_size(fd, cols + 2, rows)
os.kill(pid, __import__("signal").SIGWINCH)
pump(1.2)
final = sink[0][pre:].decode("utf-8", "replace")
try:
    # TUI4b item 10: ctrl-C is NAVIGATION from a session (back to the
    # launcher); only the second ctrl-C, now at the launcher, quits.
    os.write(fd, b"\x03")
    pump(0.4)
    os.write(fd, b"\x03")
except OSError:
    pass
probelib.drain_quiet(fd, sink)
child_clean = probelib.reap(pid)
out = sink[0]

cups = re.findall(rb"\x1b\[(\d+);(\d+)H", out)
max_row = max((int(r) for r, c in cups), default=0)
print(
    f"bytes={len(out)} alt_enter={out.count(b'\x1b[?1049h')} "
    f"alt_leave={out.count(b'\x1b[?1049l')} "
    f"max_row_addressed={max_row} clear2J={out.count(b'\x1b[2J')}"
)
rules = re.findall("─+", out.decode("utf-8", "replace"))
print("longest_rule =", max((len(r) for r in rules), default=0))
probelib.verdict(
    "PTY_PROBE_ML",
    out,
    child_clean,
    [("composer_lines_seen", all(w in final for w in ("alpha", "beta", "gamma")))],
)
