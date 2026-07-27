#!/usr/bin/env python3
"""Run `haider tui --demo` in a PTY at a given size, capture raw output,
and GATE on it (review TUI4.1 P2-3 — observations became enforced checks):
alt-screen entered and balanced, no panic, clean child exit on ⌃C, and no
row addressed beyond the frame. Metrics stay printed for the eyeball.

NB (TUI4a): the capture loop is select()-based. A quiescent-idle TUI is
correct (dirty-flag rendering, no busy redraw loop); the harness must
tolerate silence, not the other way round."""
import os, re, signal, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"
resize_to = sys.argv[4] if len(sys.argv) > 4 else None  # "COLSxROWS" mid-run

pid, fd = probelib.spawn(cols, rows, binary)
sink = [b""]
pump = probelib.make_pump(fd, sink)

deadline = time.time() + 6
resized = False
while time.time() < deadline:
    pump(0.1)
    if resize_to and not resized and time.time() > deadline - 4:
        c, r2 = resize_to.split("x")
        probelib.set_size(fd, int(c), int(r2))
        os.kill(pid, signal.SIGWINCH)
        resized = True
# Quit via ⌃C from the launcher, drain, and demand a clean exit.
try:
    os.write(fd, b"\x03")
except OSError:
    pass
probelib.drain_quiet(fd, sink)
child_clean = probelib.reap(pid)
out = sink[0]

cups = re.findall(rb"\x1b\[(\d+);(\d+)H", out)
max_row = max((int(r) for r, c in cups), default=0)
max_col = max((int(c) for r, c in cups), default=0)
clears = out.count(b"\x1b[2J")
print(
    f"bytes={len(out)} alt_enter={out.count(b'\x1b[?1049h')} "
    f"alt_leave={out.count(b'\x1b[?1049l')} "
    f"max_row_addressed={max_row} max_col_addressed={max_col} clear2J={clears}"
)
rules = re.findall("─+", out.decode("utf-8", "replace"))
print("longest_rule =", max((len(r) for r in rules), default=0))
frame_rows = max(rows, int(resize_to.split("x")[1]) if resize_to else 0)
probelib.verdict(
    "PTY_PROBE",
    out,
    child_clean,
    [
        ("painted_something", len(out) > 0),
        ("rows_within_frame", 0 < max_row <= frame_rows),
    ],
)
