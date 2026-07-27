#!/usr/bin/env python3
"""TUI4c-13b release gate: REAL across-restart persistence. Two full runs of
the built binary in a PTY sharing ONE throwaway profile dir:

  run 1 — boot to the launcher, start a session ("persist smoke test"),
          let it stream a moment, ctrl-C back to the launcher, ctrl-C to
          quit cleanly (the quit-path save).
  run 2 — boot again on the same profile and read the launcher: the
          persisted session row must be there.

GATES (review TUI4.1 P2-3): the state file exists and names the session,
both runs enter/leave the alt screen and exit CLEANLY, no panic text —
all folded into the exit code. At sizes too short to show the launcher
rows (rows < 20) the two on-screen row checks print SKIP explicitly; the
declared gate size is 118×36. This is the one behavior the headless tests
cannot fully prove — the interactive loop's own save/load wiring."""
import os, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"
# Both runs share ONE profile (probelib.spawn keeps a caller-pinned dir).
profile = tempfile.mkdtemp(prefix="haider-persist-probe-")
os.environ["HAIDER_PROFILE_DIR"] = profile


def run(drive):
    pid, fd = probelib.spawn(cols, rows, binary)
    sink = [b""]
    pump = probelib.make_pump(fd, sink)
    try:
        drive(fd, pump)
    except OSError:
        pass
    probelib.drain_quiet(fd, sink, quiet_s=1.0)
    clean = probelib.reap(pid)
    os.close(fd)
    return sink[0], clean


def first_run(fd, pump):
    pump(4.5)  # boot -> launcher (cold-build tolerant)
    os.write(fd, b"persist smoke test\r")  # start a session
    pump(3.0)  # let the turn stream (frame-saves happen throughout)
    os.write(fd, b"\x03")  # ctrl-C = navigation back to the launcher
    pump(0.8)
    os.write(fd, b"\x03")  # ctrl-C at the launcher = quit (final save)


def second_run(fd, pump):
    pump(4.5)  # boot -> launcher (hydrated)
    # Force a FULL repaint so the check reads a complete final screen.
    probelib.set_size(fd, cols + 2, rows)
    pump(1.2)
    os.write(fd, b"\x03")  # quit from the launcher


out1, clean1 = run(first_run)
state_path = os.path.join(profile, "demo-tui-state.json")
file_present = os.path.exists(state_path)
file_names_session = False
if file_present:
    with open(state_path, "rb") as handle:
        file_names_session = b"persist-smoke-test" in handle.read()

out2, clean2 = run(second_run)
text1 = out1.decode("utf-8", "replace")
text2 = out2.decode("utf-8", "replace")
print(f"run1: bytes={len(out1)} clean_exit={clean1}")
print(f"run2: bytes={len(out2)} clean_exit={clean2}")

# Row visibility is a SIZE property: short frames shed the launcher's
# recent rows (118×36 is the declared gate size for the full check).
skip = None if rows >= 20 else "SKIP"
probelib.verdict(
    "PERSISTENCE_SMOKE",
    out1 + out2,  # base checks span BOTH runs (2/2 alt, any panic anywhere)
    clean1 and clean2,
    [
        ("state_file_present", file_present),
        ("state_file_names_session", file_names_session),
        ("run1_session_started", skip or ("persist-smoke-test" in text1)),
        ("run2_launcher_shows_session", skip or ("persist-smoke-test" in text2)),
    ],
)
