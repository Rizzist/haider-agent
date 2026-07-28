#!/usr/bin/env python3
"""TUI5 item 7 — the composer cursor over a REAL PTY.

Drives the installed binary with the actual key encodings a terminal
sends and asserts the SCREEN (via forced full repaints):

- type-in-middle: "helo" + ESC[D (←) + "l" → "hello", with the CURSOR
  CELL (gold ground, 48;2;154;106;8 on dawn) present IN the composer's
  row segment — scoped to the row because the Active status badge is
  also gold-filled;
- word movement both encodings: ESC b (⌥b, what most mac terminals send)
  and ESC[1;3D (CSI ⌥←) — "one two" → "one Xtwo";
- kills: ESC[F (End) + ⌃U empties the line back to the placeholder;
- selection: ESC[1;2D (⇧←) ×2 paints the selBg band (231;215;197 dawn)
  in the composer row; ⌃C COPIES (OSC 52 with base64("me") = bWU= in the
  stream, the honest flash on screen) instead of navigating;
- history: ESC[A (↑) recalls the submitted "hello", typing extends it to
  "hello again" — proving recall puts an EDITABLE draft back.

Hermetic (probelib env scrub + throwaway profile), exit-nonzero, ladder
sizes 118×36 and 90×10. Quit is the TUI4 ⌃C ⌃C path — after stage D the
composer selection is gone, so ⌃C navigates then quits.
"""

import os
import re
import signal
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

GOLD_BG = b"48;2;154;106;8"  # dawn cursor-cell ground
SEL_BG = b"48;2;231;215;197"  # dawn selBg (maroon @0.1 over sand)

# The cursor/selection cells SPLIT styled words with SGR runs mid-word
# ("hell" + SGR + "o"), so TEXT assertions strip ANSI first; COLOR
# assertions read the raw bytes.
ANSI = re.compile(rb"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07")


def plain(chunk):
    return ANSI.sub(b"", chunk)

pid, fd = probelib.spawn(cols, rows, binary)
sink = [b""]
pump = probelib.make_pump(fd, sink)

wide = [False]


def snap():
    """Force a FULL repaint (alternate-size SIGWINCH) and return the new
    bytes — the stage's screen, not scheduler luck."""
    pre = len(sink[0])
    wide[0] = not wide[0]
    probelib.set_size(fd, cols + (2 if wide[0] else 0), rows)
    os.kill(pid, signal.SIGWINCH)
    pump(1.2)
    return sink[0][pre:]


def send(seq, pause=0.12):
    os.write(fd, seq)
    pump(pause)


def row_segment(frame, needle):
    """The repaint's RAW row segment whose ANSI-stripped text contains
    ``needle``: frames split on cursor-position moves (ESC[r;cH), each
    segment one drawn run. Returns b"" when absent — the caller's check
    then fails loudly."""
    for segment in re.split(rb"\x1b\[\d+;\d+H", frame):
        if needle in plain(segment):
            return segment
    return b""


pump(4.5)  # boot -> launcher (cold-build tolerant)
# TUI6 item 6: the LAUNCHER band's two-rule anatomy off a full repaint
# (this probe's session frames carry the session band; ml/sub carry the
# session and aura bands — the launcher rides here).
frame_l = snap()
grid_l = probelib.screen_rows(frame_l)
launcher_rows = sorted(r for r, t in grid_l.items() if "start a session" in t)
launcher_band = bool(launcher_rows) and (
    (launcher_rows[0] - 1) in grid_l
    and grid_l[launcher_rows[0] - 1].count("\u2500") >= 20
    and (launcher_rows[-1] + 1) in grid_l
    and grid_l[launcher_rows[-1] + 1].count("\u2500") >= 20
)
send(b"hi\r", 1.5)  # start a session

# Stage A — type-in-middle. "helo", ← (CSI D), "l" → "hello"; the caret
# sits ON 'o', so the composer row carries the gold cursor cell.
for ch in b"helo":
    send(bytes([ch]), 0.05)
send(b"\x1b[D")
send(b"l")
frame_a = snap()
seg_a = row_segment(frame_a, b"hell")
a_text = b"hello" in plain(frame_a)
a_cursor = GOLD_BG in seg_a
send(b"\r", 0.8)  # submit — the history entry stage D recalls

# Stage B — word movement, both ⌥ encodings, then kill-to-empty.
for ch in b"one two":
    send(bytes([ch]), 0.04)
send(b"\x1bb")  # ⌥b: ESC-prefix word-left → start of "two"
send(b"X")
frame_b = snap()
b_word = b"one Xtwo" in plain(frame_b)
send(b"\x1b[1;3D")  # CSI ⌥←: back to the start of "Xtwo"
send(b"\x1b[F")  # End
send(b"\x15", 0.3)  # ⌃U kill-to-start
frame_b2 = snap()
b_empty = b"message haider" in plain(frame_b2)  # placeholder = line emptied

# Stage C — selection band + ⌃C copies (never navigates).
for ch in b"copyme":
    send(bytes([ch]), 0.04)
send(b"\x1b[1;2D")  # ⇧←
send(b"\x1b[1;2D")  # ⇧← — selection "me"
frame_c = snap()
seg_c = row_segment(frame_c, b"copym")
c_band = SEL_BG in seg_c
pre_copy = len(sink[0])
send(b"\x03", 0.8)  # ⌃C with selection = COPY
copy_bytes = sink[0][pre_copy:]
frame_c2 = snap()
c_osc52 = b"]52;c;bWU=" in copy_bytes + frame_c2  # base64("me")
c_flash = (b"copied" in plain(frame_c2)) or (b"copy unconfirmed" in plain(frame_c2))
c_stayed = b"copym" in plain(frame_c2)  # still the session, not launcher
send(b"\x1b[F")
send(b"\x15", 0.3)  # empty the composer again

# Stage D — history: ↑ recalls "hello", typing extends the draft.
send(b"\x1b[A", 0.8)  # generous: recall must land before the snap (CI-load flake)
for ch in b" again":
    send(bytes([ch]), 0.04)
frame_d = snap()
d_recall = b"hello again" in plain(frame_d)

# Quit: ⌃C navigates (no selection now), ⌃C at the launcher quits.
try:
    send(b"\x03", 0.5)
    send(b"\x03", 0.3)
except OSError:
    pass
probelib.drain_quiet(fd, sink)
child_clean = probelib.reap(pid)
out = sink[0]

print(
    f"bytes={len(out)} alt_enter={out.count(b'\x1b[?1049h')} "
    f"alt_leave={out.count(b'\x1b[?1049l')}"
)
probelib.verdict(
    "PTY_PROBE_CURSOR",
    out,
    child_clean,
    [
        ("edit_in_middle_text", a_text),
        ("cursor_cell_gold_in_composer_row", a_cursor),
        ("word_left_esc_b_insert", b_word),
        ("ctrl_u_emptied_to_placeholder", b_empty),
        ("selection_band_in_composer_row", c_band),
        ("ctrl_c_copied_osc52_payload", c_osc52),
        ("ctrl_c_copy_flash", c_flash),
        ("ctrl_c_with_selection_stayed_in_session", c_stayed),
        ("history_recall_editable", d_recall),
        # TUI6 item 6: rule above AND below the launcher band.
        ("launcher_band_two_rules", launcher_band),
    ],
)
