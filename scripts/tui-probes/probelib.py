"""Shared harness for the PTY probe gates (review TUI4.1 P2-3).

Every probe used to print observations and exit zero; results also depended
on the host shell (an inherited NO_COLOR=1 silently stripped every SGR the
ink assertions looked for). This module makes the ladder an enforceable,
hermetic gate:

- ``spawn`` scrubs the child environment: NO_COLOR/CLICOLOR*/FORCE_COLOR
  unset, TERM pinned to xterm-256color, and a throwaway HAIDER_PROFILE_DIR
  unless the caller pinned one — results depend on the binary, never the
  invoking shell.
- ``reap`` demands a CLEAN child exit after the probe requested quit: a
  process that has to be SIGKILLed, died on a signal, or exited nonzero is
  a probe FAILURE, never background noise.
- ``verdict`` prints every check (True / False / SKIP), requires at least
  one alt-screen entry (a dead pre-alt-screen process must FAIL, not pass
  a vacuous 0 == 0 balance), folds panic text in unconditionally, and
  exits nonzero on any failure.
"""

import os
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
import time

import fcntl


def spawn(cols, rows, binary):
    """Fork the TUI under a PTY with a hermetic environment."""
    pid, fd = pty.fork()
    if pid == 0:
        # Hermetic env: the gate's result must not depend on the host
        # shell's color preferences (crossterm honors NO_COLOR — the
        # reviewer's ink assertions false-failed under an inherited
        # NO_COLOR=1).
        for var in ("NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "FORCE_COLOR", "COLORTERM"):
            os.environ.pop(var, None)
        os.environ["TERM"] = "xterm-256color"
        # TUI4c-13b: the demo persists sessions under the profile dir.
        # Isolate every probe run in a throwaway profile unless the caller
        # pinned one — gates never read or pollute real demo state.
        os.environ.setdefault(
            "HAIDER_PROFILE_DIR", tempfile.mkdtemp(prefix="haider-probe-")
        )
        os.execv(binary, [binary, "tui", "--demo"])
    set_size(fd, cols, rows)
    os.kill(pid, signal.SIGWINCH)
    return pid, fd


def set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def make_pump(fd, sink):
    """A deadline pump appending to ``sink`` (a one-element list)."""

    def pump(seconds):
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
                sink[0] += chunk

    return pump


def drain_quiet(fd, sink, quiet_s=0.8, hard_s=3.0):
    """Read until ``quiet_s`` of silence (or ``hard_s`` total)."""
    quiet = time.time()
    hard = time.time() + hard_s
    while time.time() - quiet < quiet_s and time.time() < hard:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            sink[0] += chunk
            quiet = time.time()


def reap(pid, timeout=2.5):
    """True iff the child exits ON ITS OWN with status 0 within ``timeout``.

    A child that must be SIGKILLed, dies on a signal, or exits nonzero
    reports False — probe scripts treat that as a failed check
    (review TUI4.1 P2-3c: child exit status is part of the gate).
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            done, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return False
        if done:
            return os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0
        time.sleep(0.05)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    return False


def verdict(name, out, child_clean, checks):
    """Print every check and exit: 0 iff ALL enforced checks hold.

    ``checks`` is a list of ``(label, value)`` where value is True/False or
    the string "SKIP" (bypassed by design at this size — printed loudly,
    never silently folded into a pass). Baseline checks every probe owes:
    ≥1 alt-screen entry, balanced enter/leave, no panic text, clean child
    exit.
    """
    text = out.decode("utf-8", "replace")
    alt_enter = out.count(b"\x1b[?1049h")
    alt_leave = out.count(b"\x1b[?1049l")
    panicked = ("panicked" in text) or ("RUST_BACKTRACE" in text)
    base = [
        ("alt_screen_entered", alt_enter >= 1),
        ("alt_screen_balanced", alt_enter == alt_leave),
        ("panic_free", not panicked),
        ("child_exited_cleanly", child_clean),
    ]
    ok = True
    for label, value in base + list(checks):
        if value == "SKIP":
            print(f"{label} = SKIP (by design at this size)")
            continue
        print(f"{label} = {bool(value)}")
        ok = ok and bool(value)
    print(f"{name} =", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
