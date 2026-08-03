#!/usr/bin/env python3
"""pty-probe variant that DRIVES input: start a session ("hi" + Enter),
then build a 3-line composer via Alt+Enter (ESC CR). Keystrokes are PACED
(real terminals emit Alt+Enter atomically; unpaced bursts can split at the
kernel read boundary and race ESC disambiguation), and the capture ends
with a resize-forced FULL repaint + silence drain so the check reads the
final screen, not scheduler luck. GATES (review TUI4.1 P2-3): the three
composer lines on the final screen, alt-screen balance, no panic, clean
child exit on the ⌃C ⌃C quit path.

TUI6 adds the long-single-line case (the owner's `❯ …jjjj…` report): one
logical line far past the row budget must WRAP — the row count grows, NO
ellipsis appears anywhere in the band, and the caret cell stays visible —
plus the session band's two-rule anatomy (item 6) read off the same
frame."""
import os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

GOLD_BG = b"48;2;217;181;68"  # dark cursor-cell ground (pty-probe-cursor)

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

# ---- TUI6: the long-single-line case — the composer WRAPS ----
# A fourth logical line of 200 j cells (plain ASCII is burst-safe; only
# ESC sequences need pacing). No submit: the draft itself must wrap.
os.write(fd, b"\x1b\r")
pump(0.15)
os.write(fd, b"j" * 200)
pump(1.0)
pre2 = len(sink[0])
probelib.set_size(fd, cols, rows)  # toggle back: full repaint at cols
os.kill(pid, __import__("signal").SIGWINCH)
pump(1.2)
wrap_frame = sink[0][pre2:]
grid = probelib.screen_rows(wrap_frame)
j_rows = sorted(r for r, t in grid.items() if "jjjj" in t)
# The caret cell (gold ground) must sit ON a wrapped j row — scoped to
# the row's raw segment because the status badge is also gold-filled.
caret_seen = any(
    b"jjjj" in probelib.plain(seg) and GOLD_BG in seg
    for seg in re.split(rb"\x1b\[\d+;\d+H", wrap_frame)
)
# Band anatomy (TUI6 item 6), session surface: the gold rule directly
# above the band's first visible row, the closing frame rule within the
# pad rows under its last. The closing rule needs spare height — at the
# 90x10 rung the ledger sheds it, so that half gates on the tall run.
composer_rows = sorted(
    r
    for r, t in grid.items()
    if "jjjj" in t or any(w in t for w in ("alpha", "beta", "gamma"))
)


def rule_row(r):
    return r in grid and grid[r].count("─") >= 20


band_rule_above = bool(composer_rows) and rule_row(composer_rows[0] - 1)
band_rule_below = bool(j_rows) and any(rule_row(j_rows[-1] + d) for d in (1, 2, 3))
tall = rows >= 30

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
    [
        ("composer_lines_seen", all(w in final for w in ("alpha", "beta", "gamma"))),
        # TUI6 items 1-2: growth by WRAPPING — more than one visual row of
        # j's, no ellipsis on any of them, the caret cell visible.
        ("wrap_rows_grow", len(j_rows) >= 2),
        (
            "wrap_no_ellipsis",
            bool(j_rows) and all("…" not in grid[r] for r in j_rows),
        ),
        ("wrap_caret_visible", caret_seen),
        # TUI6 item 6: the session band's two-rule anatomy.
        ("band_rule_above", band_rule_above),
        ("band_rule_below", band_rule_below if tall else "SKIP"),
    ],
)
