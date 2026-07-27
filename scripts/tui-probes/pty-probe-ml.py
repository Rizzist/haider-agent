#!/usr/bin/env python3
"""pty-probe variant that DRIVES input: start a session ("hi" + Enter),
then build a 3-line composer via Alt+Enter (ESC CR). Keystrokes are PACED
(real terminals emit Alt+Enter atomically; unpaced bursts can split at the
kernel read boundary and race ESC disambiguation), and the capture ends
with a resize-forced FULL repaint + silence drain so the check reads the
final screen, not scheduler luck. Reports the same metrics as pty-probe.py
plus whether the composer lines appear in the final screen."""
import os, pty, sys, tempfile, time, fcntl, termios, struct, signal, re, select

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

pid, fd = pty.fork()
if pid == 0:
    # TUI4c-13b: the demo persists sessions under the profile dir now.
    # Isolate every probe run in a throwaway profile unless the caller
    # pinned one — gates must never read or pollute real demo state.
    os.environ.setdefault("HAIDER_PROFILE_DIR", tempfile.mkdtemp(prefix="haider-probe-"))
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

pump(3.2)                      # boot -> launcher
os.write(fd, b"hi\r")          # start a session
pump(1.5)
for part in (b"alpha", b"\x1b\r", b"beta", b"\x1b\r", b"gamma"):
    os.write(fd, part)         # Alt+Enter = ESC CR, delivered atomically
    pump(0.12)
pump(1.0)
# Force a FULL final repaint so the check reads the final screen.
pre = len(out)
set_size(fd, cols + 2, rows)
os.kill(pid, signal.SIGWINCH)
pump(1.2)
final = out[pre:].decode("utf-8", "replace")
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

alt_enter = out.count(b"\x1b[?1049h")
alt_leave = out.count(b"\x1b[?1049l")
cups = re.findall(rb"\x1b\[(\d+);(\d+)H", out)
max_row = max((int(r) for r, c in cups), default=0)
clears = out.count(b"\x1b[2J")
text = out.decode("utf-8", "replace")
rules = re.findall("─+", text)
print(f"bytes={len(out)} alt_enter={alt_enter} alt_leave={alt_leave} "
      f"max_row_addressed={max_row} clear2J={clears}")
print("longest_rule =", max((len(r) for r in rules), default=0))
print("composer_lines_seen =", all(w in final for w in ("alpha", "beta", "gamma")))
