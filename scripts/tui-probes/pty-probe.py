#!/usr/bin/env python3
"""Run `haider tui --demo` in a PTY at a given size, capture raw output,
report: alt-screen usage, max row/col addressed, resize behavior.

NB (TUI4a): the capture loop is select()-based. The old loop used a bare
blocking os.read and only checked its deadline BETWEEN reads, which was
fine while the launcher auto-played a demo turn at t=6s (output never
stopped for long). TUI4a removes auto-play (owner item 1): an untouched
launcher is quiescent after boot (~3s), a blocking read never returns,
and the probe hung forever while the app sat healthy. A quiescent-idle
TUI is correct (dirty-flag rendering, no busy redraw loop); the harness
must tolerate silence, not the other way round."""
import os, pty, sys, time, fcntl, termios, struct, signal, re, select

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"
resize_to = sys.argv[4] if len(sys.argv) > 4 else None  # "COLSxROWS" mid-run

pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "tui", "--demo"])

def set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

set_size(fd, cols, rows)
os.kill(pid, signal.SIGWINCH)

out = b""
deadline = time.time() + 6
resized = False
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.1)
    if r:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    if resize_to and not resized and time.time() > deadline - 4:
        c, r2 = resize_to.split("x")
        set_size(fd, int(c), int(r2))
        os.kill(pid, signal.SIGWINCH)
        resized = True
# quit via Ctrl+C byte, then drain until 0.8s of silence
try:
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

alt_enter = out.count(b"\x1b[?1049h")
alt_leave = out.count(b"\x1b[?1049l")
cups = re.findall(rb"\x1b\[(\d+);(\d+)H", out)
max_row = max((int(r) for r, c in cups), default=0)
max_col = max((int(c) for r, c in cups), default=0)
clears = out.count(b"\x1b[2J")
print(f"bytes={len(out)} alt_enter={alt_enter} alt_leave={alt_leave} "
      f"max_row_addressed={max_row} max_col_addressed={max_col} clear2J={clears}")
# widest run of the horizontal rule char '─' seen in output
rules = re.findall("─+", out.decode("utf-8", "replace"))
print("longest_rule =", max((len(r) for r in rules), default=0))
