#!/usr/bin/env python3
"""TUI4d item 14 release gate: the animations are ALIVE and BOUNDED. Sit at
the launcher (the L1 seed's live chip keeps its row busy, so the gold dot
pulses and the rail shimmers) for a quiet measurement window with zero
input, then assert:

  - the screen keeps updating (bytes > floor — the phase clock ticks);
  - output stays BOUNDED (bytes < cap — phase flips redraw only the
    changed cells at ~1.7 Hz; a runaway full-repaint loop would emit
    hundreds of KB in the window);
  - both shimmer inks appear in the window (gold AND maroon foregrounds —
    the rail actually cycles, Desert Dawn tokens);
  - balanced alt-screen enter/leave, no panic text.

The idle-stillness half (zero wakeups with nothing animated) is proven
headlessly by tui4d_animation_tests — a PTY launcher is never idle (the
seeded L1 row is busy by design), so this probe measures the live half."""
import os, pty, sys, tempfile, time, fcntl, termios, struct, signal, select

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

pid, fd = pty.fork()
if pid == 0:
    os.environ.setdefault("HAIDER_PROFILE_DIR", tempfile.mkdtemp(prefix="haider-probe-"))
    os.execv(binary, [binary, "tui", "--demo"])


def set_size(fd, c, r):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", r, c, 0, 0))


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


pump(4.5)  # boot -> launcher (cold-build tolerant)
pump(1.5)  # settle: drain any boot tail before measuring
# The predicate is STATE-based, not viewport-based: at heights where the
# launcher sheds its recent rows the clock still ticks, but every frame
# diffs to nothing — the window then carries only per-tick reset chatter
# (~40 bytes/tick), and the ink checks are meaningless. Gate them on the
# pulsing row actually being on screen.
row_visible = b"l1-remote-projects" in out
window_start = len(out)
WINDOW_S = 6.0
pump(WINDOW_S)  # the measurement window — ZERO input
window = out[window_start:]

try:
    os.write(fd, b"\x03")  # quit from the launcher
except OSError:
    pass
quiet = time.time()
while time.time() - quiet < 1.5:
    r, _, _ = select.select([fd], [], [], 0.1)
    if r:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
        quiet = time.time()
try:
    os.kill(pid, signal.SIGKILL)
except ProcessLookupError:
    pass
try:
    os.waitpid(pid, 0)
except ChildProcessError:
    pass

text = out.decode("utf-8", "replace")
# Desert Dawn inks (theme.rs): gold #9a6a08, maroon #7c2d12.
gold = b"154;106;8" in window
maroon = b"124;45;18" in window
alive = len(window) > 40
CAP = 40_000  # ~10 diff-only flips ≈ 2KB; a repaint storm is 150KB+
bounded = len(window) < CAP
print(
    f"bytes_total={len(out)} window_bytes={len(window)} window_s={WINDOW_S}"
    f" alt_enter={out.count(b'\x1b[?1049h')} alt_leave={out.count(b'\x1b[?1049l')}"
)
print("panic_text =", ("panicked" in text) or ("RUST_BACKTRACE" in text))
print("pulsing_row_visible =", row_visible)
print("animates_while_quiescent =", alive)
print(f"bounded_output(<{CAP}) =", bounded)
print("shimmer_gold_seen =", gold)
print("shimmer_maroon_seen =", maroon)
ok = (
    bounded
    and (not row_visible or (alive and gold and maroon))
    and out.count(b"\x1b[?1049h") == out.count(b"\x1b[?1049l")
    and "panicked" not in text
)
print("ANIMATION_PROBE =", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
