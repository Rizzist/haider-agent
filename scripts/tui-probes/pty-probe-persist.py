#!/usr/bin/env python3
"""TUI4c-13b release gate: REAL across-restart persistence. Two full runs of
the built binary in a PTY sharing ONE throwaway profile dir:

  run 1 — boot to the launcher, start a session ("persist smoke test"),
          let it stream a moment, ctrl-C back to the launcher, ctrl-C to
          quit cleanly (the quit-path save).
  run 2 — boot again on the same profile and read the launcher: the
          persisted session row must be there.

Reports alt-screen balance and panic text for both runs, whether
demo-tui-state.json exists (and names the session), and whether the second
launcher shows the row. This is the one behavior the headless tests cannot
fully prove — the interactive loop's own save/load wiring."""
import os, pty, sys, tempfile, time, fcntl, termios, struct, signal, re, select

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"
profile = tempfile.mkdtemp(prefix="haider-persist-probe-")


def set_size(fd, c, r):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", r, c, 0, 0))


def run(drive):
    """One PTY run of `haider tui --demo` on the shared profile; `drive(fd,
    pump)` scripts the input. Returns (bytes, exit_status)."""
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["HAIDER_PROFILE_DIR"] = profile
        os.execv(binary, [binary, "tui", "--demo"])
    set_size(fd, cols, rows)
    os.kill(pid, signal.SIGWINCH)
    out = [b""]

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
                out[0] += chunk

    try:
        drive(fd, pump)
    except OSError:
        pass
    # Drain to EOF (the app should be exiting after the final ctrl-C).
    quiet = time.time()
    while time.time() - quiet < 2.0:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out[0] += chunk
            quiet = time.time()
    try:
        _, status = os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        status = 0
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    os.close(fd)
    return out[0], status


def first_run(fd, pump):
    # 4.5s not 3.2: a cold post-build binary can outpace a 3.2s pump (the
    # documented pty-probe-sub flake) and the typed line would land on the
    # boot screen, starting no session.
    pump(4.5)  # boot -> launcher
    os.write(fd, b"persist smoke test\r")  # start a session
    pump(3.0)  # let the turn stream (frame-saves happen throughout)
    os.write(fd, b"\x03")  # ctrl-C = navigation back to the launcher
    pump(0.8)
    os.write(fd, b"\x03")  # ctrl-C at the launcher = quit (final save)


def second_run(fd, pump):
    pump(4.5)  # boot -> launcher (hydrated)
    # Force a FULL repaint so the check reads a complete final screen.
    set_size(fd, cols + 2, rows)
    pump(1.2)
    os.write(fd, b"\x03")  # quit from the launcher


out1, status1 = run(first_run)
state_path = os.path.join(profile, "demo-tui-state.json")
file_present = os.path.exists(state_path)
file_names_session = False
if file_present:
    with open(state_path, "rb") as handle:
        file_names_session = b"persist-smoke-test" in handle.read()

out2, status2 = run(second_run)

def report(tag, out):
    text = out.decode("utf-8", "replace")
    print(
        f"{tag}: bytes={len(out)}"
        f" alt_enter={out.count(b'\x1b[?1049h')} alt_leave={out.count(b'\x1b[?1049l')}"
        f" panic_text={('panicked' in text) or ('RUST_BACKTRACE' in text)}"
    )
    return text


text1 = report("run1", out1)
text2 = report("run2", out2)
print("run1_session_started =", "persist-smoke-test" in text1)
print("state_file_present =", file_present)
print("state_file_names_session =", file_names_session)
print("run2_launcher_shows_session =", "persist-smoke-test" in text2)
ok = (
    file_present
    and file_names_session
    and "persist-smoke-test" in text1
    and "persist-smoke-test" in text2
    and out1.count(b"\x1b[?1049h") == out1.count(b"\x1b[?1049l")
    and out2.count(b"\x1b[?1049h") == out2.count(b"\x1b[?1049l")
)
print("PERSISTENCE_SMOKE =", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
