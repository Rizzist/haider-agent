#!/usr/bin/env python3
"""TUI3b commit-2 probe: drive the REAL runtime through the subagent tree and
the aura stage. Starts a two-subagent turn, waits for the chips + the amber ?
question, answers the chip card by digit, then opens /aura, toggles the engine
and mute, fires hold-to-talk, and escapes back. Reports alt-screen balance,
panic text, and whether the new surfaces actually painted."""
import os, pty, sys, time, fcntl, termios, struct, signal, re, select

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "tui", "--demo"])


def set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


set_size(fd, cols, rows)
os.kill(pid, signal.SIGWINCH)

out = b""


def pump(seconds):
    global out
    deadline = time.time() + seconds
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            out += chunk


def mark():
    return len(out)


def since(pre):
    return out[pre:].decode("utf-8", "replace")


pump(3.2)  # boot -> launcher
os.write(fd, b"use two subagents to split this work\r")
pump(9.0)  # parent turn + both child scripts up to their cards
sub_paint = since(0)
# The tests chip is holding its amber ? — the parent turn is idle by now.
pre = mark()
# TUI4c: an idle esc now really DETACHES (the session checks into its
# slot), so /aura is typed from the SESSION — entered that way, esc from
# the aura returns to it (sim: exit goes back to where you came from).
os.write(fd, b"/aura\r")
pump(1.2)
aura_paint = since(pre)
os.write(fd, b"spin up billing on workstation and run its tests\r")
pump(4.5)
pre2 = mark()
os.write(fd, b"\x1b")  # exit aura
pump(0.8)
back_paint = since(pre2)
# TUI4b item 11: wheel into history — the sticky origin band pins the
# producing prompt on the barBg band (Desert Dawn barBg = rgb(237,225,207);
# only the /help overlay shares that ground and /help is never opened
# here). The band pins at sizes where the transcript actually overflows —
# 90x10 is the pinning run; at tall frames the turn fits and no sticky is
# BY DESIGN.
pre4 = mark()
for _ in range(10):
    os.write(fd, b"\x1b[<64;40;8M")  # SGR mouse wheel-up
    pump(0.08)
pump(0.5)
scrolled = since(pre4)
# Force a full final repaint.
pre3 = mark()
set_size(fd, cols + 2, rows)
os.kill(pid, signal.SIGWINCH)
pump(1.5)
final = since(pre3)
try:
    # TUI4b item 10: ctrl-C is NAVIGATION from a session (back to the
    # launcher); only the second ctrl-C, now at the launcher, quits.
    os.write(fd, b"\x03")
    pump(0.4)
    os.write(fd, b"\x03")
    quiet = time.time()
    while time.time() - quiet < 0.8:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            out += chunk
            quiet = time.time()
except OSError:
    pass
try:
    os.kill(pid, signal.SIGKILL)
except ProcessLookupError:
    pass

text = out.decode("utf-8", "replace")
alt_enter = out.count(b"\x1b[?1049h")
alt_leave = out.count(b"\x1b[?1049l")
cups = re.findall(rb"\x1b\[(\d+);(\d+)H", out)
max_row = max((int(r) for r, c in cups), default=0)
print(
    f"bytes={len(out)} alt_enter={alt_enter} alt_leave={alt_leave} max_row_addressed={max_row}"
)
print("panic_text =", ("panicked" in text) or ("RUST_BACKTRACE" in text))
print("subtree_header_painted =", "subagents" in sub_paint)
print("chip_glyph_painted =", ("├─" in sub_paint) or ("└─" in sub_paint))
print("amber_question_painted =", "testcontainers" in sub_paint)
print("aura_bar_painted =", "AURA" in aura_paint)
print("aura_orb_painted =", ("IDLE" in aura_paint) or ("THINKING" in aura_paint))
print("aura_spawn_logged =", "tests green" in text)
# NB: the header's product/version are separate styled spans, so an SGR
# escape splits "haider v" — the session line's dim run is contiguous.
print("back_to_session =", "branch main" in final)
print("final_has_subtree =", "subagents" in final)
print("sticky_prompt_pinned =", "use two subagents" in scrolled)
print("sticky_band_ground =", "48;2;237;225;207" in scrolled)
