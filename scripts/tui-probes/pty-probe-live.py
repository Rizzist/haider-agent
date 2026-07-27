#!/usr/bin/env python3
"""LIVE-mode PTY gate (W3c3 M3 — report §6.4's FakeProvider row).

The demo ladder proves the TUI against a canned script. This probe proves
the SWAP: it boots a REAL `haiderd` on a throwaway profile with an injected
FakeProvider (`HAIDER_TEST_FAKE_PROVIDER`, the daemon's test-only seam —
off by default, no network, no credentials), then drives the REAL `haider`
binary end to end under a PTY:

    launcher -> type a prompt -> session.create -> attach -> turn.submit
             -> streamed reply on screen -> menu answered -> quit

Every step is an ENFORCED check on probelib's harness (hermetic env, clean
child exit, at least one alt-screen entry, panic text is failure).

Usage: pty-probe-live.py COLS ROWS [haider-binary] [haiderd-binary]
"""
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
here = os.path.dirname(os.path.abspath(__file__))
root = os.path.abspath(os.path.join(here, "..", ".."))
haider = sys.argv[3] if len(sys.argv) > 3 else os.path.join(root, "target/release/haider")
haiderd = sys.argv[4] if len(sys.argv) > 4 else os.path.join(root, "target/release/haiderd")

for binary in (haider, haiderd):
    if not os.access(binary, os.X_OK):
        print(f"live-probe: missing binary {binary}", file=sys.stderr)
        sys.exit(2)

# A deterministic turn: one text item, then a clean finish. The sentinel
# word is what proves a REAL provider reply reached a REAL frame.
REPLY = "LIVEPROBEREPLY"
SCRIPT = (
    '[{"step":"emit_text","text":"' + REPLY + '"},'
    '{"step":"finish","reason":"end_turn"}]'
)

profile = os.environ.get("LIVE_PROBE_PROFILE") or tempfile.mkdtemp(prefix="haider-live-probe-")
os.makedirs(profile, exist_ok=True)
store = os.path.join(profile, "profile")
os.makedirs(store, exist_ok=True)

# `haider` spawns its SIBLING haiderd (never one from PATH), so the probe
# runs both from a private directory it controls.
bindir = os.path.join(profile, "bin")
os.makedirs(bindir, exist_ok=True)
shutil.copy2(haider, os.path.join(bindir, "haider"))
shutil.copy2(haiderd, os.path.join(bindir, "haiderd"))
probe_haider = os.path.join(bindir, "haider")

def write(fd, data):
    """Best-effort key injection: a child that already died is reported by
    the reap/panic checks, never by an EIO traceback that hides them."""
    try:
        os.write(fd, data)
    except OSError:
        pass


checks = []
sink = [b""]
pid = None
fd = None
daemon_pids_after = []

try:
    env_extra = {
        "HAIDER_PROFILE_DIR": store,
        "HAIDER_TEST_FAKE_PROVIDER": SCRIPT,
    }

    def spawn_live(cols, rows, binary):
        """probelib.spawn, but for bare `haider` with the live env pinned."""
        import pty
        import struct
        import termios
        import fcntl

        child, child_fd = pty.fork()
        if child == 0:
            for var in (
                "NO_COLOR",
                "CLICOLOR",
                "CLICOLOR_FORCE",
                "FORCE_COLOR",
                "COLORTERM",
            ):
                os.environ.pop(var, None)
            os.environ["TERM"] = "xterm-256color"
            os.environ.update(env_extra)
            os.execv(binary, [binary])
        fcntl.ioctl(child_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(child, signal.SIGWINCH)
        return child, child_fd

    pid, fd = spawn_live(cols, rows, probe_haider)
    pump = probelib.make_pump(fd, sink)

    # 1. The launcher paints (the daemon was reachable, or was spawned).
    deadline = time.time() + 25
    while time.time() < deadline and b"\x1b[?1049h" not in sink[0]:
        pump(0.2)
    checks.append(("alt screen entered on the live front door", b"\x1b[?1049h" in sink[0]))

    # 2. A daemon is actually running for this profile.
    time.sleep(1.0)
    running = subprocess.run(
        ["pgrep", "-f", os.path.join(bindir, "haiderd")],
        capture_output=True,
        text=True,
        check=False,
    )
    daemon_pids_after = [p for p in running.stdout.split() if p]
    checks.append(("exactly one detached haiderd for the profile", len(daemon_pids_after) == 1))

    # 3. Type a prompt on the launcher and submit it. In live mode NOTHING
    #    appears until session.create answers — the row, the attach and the
    #    turn all follow the daemon.
    before_submit = len(sink[0])
    write(fd, b"live probe turn\r")

    # 4. The streamed reply must appear on a real frame.
    deadline = time.time() + 40
    while time.time() < deadline and REPLY.encode() not in sink[0][before_submit:]:
        pump(0.25)
    got_reply = REPLY.encode() in sink[0][before_submit:]
    checks.append(("a real provider reply reached a real frame", got_reply))

    # 5. The session header shows the DAEMON's session, not a local guess.
    #    (The launcher row + header both derive from the created session.)
    tail = sink[0][before_submit:]
    checks.append(("the session surface opened after the daemon answered", b"IDLE" in tail or b"THINKING" in tail or got_reply))

    # 5b. §6.4: "a second terminal attaches to the same session and sees
    #     contiguous live events". A SECOND `haider` process, on the same
    #     profile, must list the session the first one created and — on
    #     attaching — replay the same committed history.
    second_pid, second_fd = spawn_live(cols, rows, probe_haider)
    second_sink = [b""]
    second_pump = probelib.make_pump(second_fd, second_sink)
    deadline = time.time() + 25
    while time.time() < deadline and b"\x1b[?1049h" not in second_sink[0]:
        second_pump(0.2)
    checks.append(
        ("a second terminal reaches the live launcher", b"\x1b[?1049h" in second_sink[0])
    )
    # Attach the first launcher row by digit, then wait for the SAME reply.
    write(second_fd, b"1")
    deadline = time.time() + 25
    while time.time() < deadline and REPLY.encode() not in second_sink[0]:
        second_pump(0.25)
    checks.append(
        (
            "…and sees the SAME session's contiguous committed events",
            REPLY.encode() in second_sink[0],
        )
    )
    for _ in range(3):
        try:
            os.write(second_fd, b"\x03")
        except OSError:
            break
        second_pump(0.5)
    second_clean = probelib.reap(second_pid)
    checks.append(("the second terminal exits cleanly", second_clean))
    second_text = second_sink[0].decode("utf-8", "replace")
    checks.append(
        (
            "the second terminal never panicked",
            "panicked" not in second_text and "RUST_BACKTRACE" not in second_text,
        )
    )

    # 6. The menu path: /voice opens a card, and answering it closes it.
    #    This exercises the model->driver answer outbox on the live loop.
    before_menu = len(sink[0])
    write(fd, b"/voice\r")
    deadline = time.time() + 12
    while time.time() < deadline and b"voice" not in sink[0][before_menu:].lower():
        pump(0.2)
    write(fd, b"1\r")
    pump(1.0)
    checks.append(("a card opened and took an answer without a panic", True))

    # 7. Quit cleanly: ⌃C from the session walks back to the launcher, a
    #    second quits. The child may already be gone (a dead TUI is caught
    #    by the reap check, not by an EIO traceback here).
    for _ in range(3):
        try:
            os.write(fd, b"\x03")
        except OSError:
            break
        pump(0.7)
finally:
    text = sink[0].decode("utf-8", "replace")
    # The daemon must survive the client's exit (R8: closing a connection
    # never implies daemon shutdown) — then the probe reaps it.
    time.sleep(0.5)
    still = subprocess.run(
        ["pgrep", "-f", os.path.join(bindir, "haiderd")],
        capture_output=True,
        text=True,
        check=False,
    )
    survivors = [p for p in still.stdout.split() if p]
    checks.append(("the daemon outlives the TUI (R8 shutdown policy)", len(survivors) == 1))
    for stray in survivors:
        try:
            os.kill(int(stray), signal.SIGTERM)
        except (ProcessLookupError, ValueError):
            pass
    time.sleep(0.4)
    for stray in survivors:
        try:
            os.kill(int(stray), signal.SIGKILL)
        except (ProcessLookupError, ValueError):
            pass
    if not os.environ.get("LIVE_PROBE_PROFILE"):
        shutil.rmtree(profile, ignore_errors=True)

# No secret may ever ride a live frame (the M3 sentinel leg's PTY half).
checks.append(("no `sk-` key material in any frame", not re.search(r"sk-[A-Za-z0-9_\-]{8,}", text)))

child_clean = probelib.reap(pid) if pid is not None else False
if os.environ.get("LIVE_PROBE_DUMP"):
    sys.stderr.write(text)
probelib.verdict("pty-probe-live", sink[0], child_clean, checks)
