#!/usr/bin/env python3
"""TUI4d item 14 release gate: the animations are ALIVE and BOUNDED. Sit at
the launcher (the L1 seed's live chip keeps its row busy, so the gold dot
pulses and the rail shimmers) for a quiet measurement window with zero
input, then GATE (review TUI4.1 P2-3):

  - output stays BOUNDED two ways: a byte cap (a repaint storm is 150KB+
    against ~2KB of diff-only flips) AND a frame-flip ceiling on the
    window (one cursor-hide per drawn frame — a diff/reset storm that
    stays under the byte cap still busts the flip count);
  - when the pulsing row is on screen: the screen keeps updating (the
    phase clock ticks) and BOTH shimmer inks (gold and maroon) cross the
    wire; when the size sheds the row those checks print SKIP explicitly;
  - alt screen entered and balanced, no panic, clean child exit —
    a dead pre-alt-screen process FAILS, it cannot pass a vacuous 0 == 0.

The idle-stillness half (zero wakeups with nothing animated) is proven
headlessly by tui4d_animation_tests — a PTY launcher is never idle (the
seeded L1 row is busy by design), so this probe measures the live half.
The environment is scrubbed by probelib (NO_COLOR stripped, TERM pinned)
so the ink assertions cannot false-fail on host shell color settings."""
import os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

pid, fd = probelib.spawn(cols, rows, binary)
sink = [b""]
pump = probelib.make_pump(fd, sink)

pump(4.5)  # boot -> launcher (cold-build tolerant)
pump(1.5)  # settle: drain any boot tail before measuring
# The predicate is STATE-based, not viewport-based: at heights where the
# launcher sheds its recent rows the clock still ticks, but every frame
# diffs to nothing — the window then carries only per-tick reset chatter,
# and the ink/liveness checks are meaningless (SKIP, printed loudly).
row_visible = b"l1-remote-projects" in sink[0]
window_start = len(sink[0])
WINDOW_S = 6.0
pump(WINDOW_S)  # the measurement window — ZERO input
window = sink[0][window_start:]

try:
    os.write(fd, b"\x03")  # quit from the launcher
except OSError:
    pass
probelib.drain_quiet(fd, sink)
child_clean = probelib.reap(pid)
out = sink[0]

# Dark-theme shimmer inks (theme.rs DARK): gold #d9b544, ember #e57a4a.
gold = b"217;181;68" in window
maroon = b"229;122;74" in window
# One cursor-hide per drawn frame: ~10 phase flips fit the 6s window; a
# storm that redraws every frame tick is an order of magnitude over.
flips = window.count(b"\x1b[?25l")
BYTE_CAP = 40_000
FLIP_CAP = 25
print(
    f"bytes_total={len(out)} window_bytes={len(window)} window_s={WINDOW_S} "
    f"window_flips={flips} row_visible={row_visible}"
)
skip = None if row_visible else "SKIP"
probelib.verdict(
    "ANIMATION_PROBE",
    out,
    child_clean,
    [
        (f"bounded_bytes(<{BYTE_CAP})", len(window) < BYTE_CAP),
        (f"bounded_flips(<={FLIP_CAP})", flips <= FLIP_CAP),
        ("animates_while_quiescent", skip or (len(window) > 40 and flips >= 2)),
        ("shimmer_gold_seen", skip or gold),
        ("shimmer_maroon_seen", skip or maroon),
    ],
)
