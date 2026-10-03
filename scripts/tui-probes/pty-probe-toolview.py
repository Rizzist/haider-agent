#!/usr/bin/env python3
"""973-tui-toolview PTY gate: a tool row EXPANDS to its full detail and Esc
comes back to exactly the same screen.

Drives the real `haider tui --demo` binary under a PTY (hermetic env from
probelib): start the interactive demo's scripted turn from the launcher, wait for its
Claude-Code-style rows (`● Search(…)`, `● Read(…)`, `● Edit(…)`,
`● Bash(cargo check)` over `⎿ (No output)`), focus the newest tool row
(the Bash call) with the typed fallback
(`/collapse next` — Alt-free), press ⌃O, and GATE on:

  - the full-detail view paints (`full detail`, the row's header, the
    `esc / ⌃O back` hint);
  - Esc closes it and a forced FULL repaint of the session equals the one
    taken before ⌃O, row for row (the transcript was never scrolled);
  - ⌃O a second time reopens it and ⌃O closes it again ("the same key");
  - the budget tally never appears; alt screen balanced, no panic, clean
    child exit on ⌃C.

Usage: pty-probe-toolview.py COLS ROWS [haider-binary]
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

pid, fd = probelib.spawn(cols, rows, binary)
sink = [b""]
pump = probelib.make_pump(fd, sink)
checks = []


def write(data):
    try:
        os.write(fd, data)
    except OSError:
        pass


def repaint():
    """One forced FULL frame (a size change repaints every cell), parsed to
    per-row text. ratatui paints diffs, so presence/absence is only
    observable on a full frame."""
    probelib.set_size(fd, cols, rows - 1)
    pump(0.7)
    mark = len(sink[0])
    probelib.set_size(fd, cols, rows)
    pump(1.3)
    return probelib.screen_rows(sink[0][mark:])


def screen_text(screen):
    return "\n".join(screen[row] for row in sorted(screen))


def wait_screen(needle, seconds):
    deadline = time.time() + seconds
    screen = {}
    while time.time() < deadline:
        pump(0.5)
        screen = repaint()
        if needle in screen_text(screen):
            return screen
    return screen


pump(4.5)  # boot -> launcher
write(b"probe the tool rows\r")
session = wait_screen("● Bash(cargo check)", 25)
text = screen_text(session)
checks.append(("the demo turn reached the session", "● Read(" in text))
checks.append(("a search reads as ● Search(pattern)", "● Search(" in text))
checks.append(("an edit reads as ● Edit(path)", "● Edit(" in text))
checks.append(("a shell call reads as ● Bash(cmd)", "● Bash(cargo check)" in text))
checks.append(("its ⎿ line reports the result", "⎿ (No output)" in text))
checks.append(("no request-budget tally", "tranche" not in text))

# Focus the newest tool row without an Alt chord, then settle.
write(b"/collapse next\r")
pump(1.0)
before = repaint()

write(b"\x0f")  # ⌃O on the focused row
pump(1.0)
view = repaint()
view_text = screen_text(view)
checks.append(("⌃O opened the full-detail view", "full detail" in view_text))
checks.append(("the view names its way back", "esc / ⌃O back" in view_text))
checks.append(("the view shows the focused row's header", "● Bash(cargo check)" in view_text))

write(b"\x1b")  # Esc
pump(1.5)  # crossterm's lone-Esc disambiguation window
after = repaint()
after_text = screen_text(after)
checks.append(("Esc closed the view", "full detail" not in after_text))
checks.append(("Esc returned to the identical screen", after == before))
if after != before:
    for row in sorted(set(before) | set(after)):
        if before.get(row) != after.get(row):
            print(f"row {row}: before={before.get(row)!r} after={after.get(row)!r}")

write(b"\x0f")
pump(1.0)
again = screen_text(repaint())
write(b"\x0f")
pump(1.0)
closed = repaint()
checks.append(("⌃O reopens the view", "full detail" in again))
checks.append(("…and the same key closes it", closed == before))

if os.environ.get("TOOLVIEW_PROBE_DUMP"):
    print("---- session ----")
    print(text)
    print("---- detail view ----")
    print(view_text)
    print("---- after esc ----")
    print(after_text, flush=True)

# The session surface's ⌃C is two-step (first arms, second quits) — the
# convention the other session probes follow.
write(b"\x03")
pump(0.5)
write(b"\x03")
probelib.drain_quiet(fd, sink)
child_clean = probelib.reap(pid)
out = sink[0]
probelib.verdict("TOOLVIEW_PROBE", out, child_clean, checks)
