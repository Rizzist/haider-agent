#!/usr/bin/env python3
"""LIVE-mode PTY gate (W3c3 M3 — report §6.4's FakeProvider row).

The demo ladder proves the TUI against a canned script. This probe proves
the SWAP: it boots a REAL `haiderd` on a throwaway profile with an injected
FakeProvider (`HAIDER_TEST_FAKE_PROVIDER`, the daemon's test-only seam —
off by default, no network, no credentials), then drives the REAL `haider`
binary end to end under a PTY:

    launcher -> type a prompt -> session.create -> attach -> turn.submit
             -> streamed reply on screen
             -> a SECOND terminal attaches and replays the same history
             -> a REAL daemon `request_input` resolves automatically
             -> both attached terminals show its tool-result continuation,
                with NO pending card
             -> a THIRD, COLD terminal reconstructs the continuation from
                committed history, still with NO pending card
             -> quit

v0.0.970 defaults to autonomous interaction. `FakeStep::EmitRequestInput`
(JSON `emit_request_input`) must return exactly one tool result without
opening, answering, or resolving a menu. `expect_tool_result` refuses the
continuation unless that result actually reaches the next provider request.
The journal and forced full frames enforce this contract independently.

Every step is an ENFORCED check on probelib's harness (hermetic env, clean
child exit, at least one alt-screen entry, panic text is failure).

Usage: pty-probe-live.py COLS ROWS [haider-binary] [haiderd-binary]
Env:   LIVE_PROBE_DUMP=1 dumps every captured stream to stderr;
       LIVE_PROBE_PROFILE=<dir> keeps the throwaway profile (its
       `daemon.log` and `store.sqlite` are where daemon-side causes live).
"""
import json
import os
import re
import shutil
import signal
import sqlite3
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
haider_tui = os.path.join(os.path.dirname(os.path.abspath(haider)), "haider-tui")

for binary in (haider, haiderd, haider_tui):
    if not os.access(binary, os.X_OK):
        print(f"live-probe: missing binary {binary}", file=sys.stderr)
        sys.exit(2)

# REPLY identifies the first streamed turn. CARD and PICK identify forbidden
# request_input UI. RESUMED proves the provider received CALL's automatic tool
# result, including on live peers and a cold attachment replaying the journal.
REPLY = "LIVEPROBEREPLY"
CARD = "LIVEPROBECARD"
PICK = "LIVEPROBEPICK"
RESUMED = "LIVEPROBERESUMED"
CALL = "live-probe-input-1"

# Three FakeProvider SEGMENTS (`next_segment` cuts at every terminal step),
# so the probe — not a sleep — decides when each hop happens:
#   1. turn one: one text item, clean finish.
#   2. turn two: the `request_input` call, finished with `tool_use`. The
#      autonomous actor returns a tool result without opening a menu.
#   3. the RESUMPTION, gated by `expect_tool_result`: the fake provider
#      REFUSES this segment (typed Internal error) unless the request
#      actually carries the tool result for `CALL`, so RESUMED on screen can
#      only mean the result completed the round trip.
# The script is FINITE and ends here: a worker that resumed twice would run
# a turn with no segment left, and a stream that ends without a finish event
# is journaled as `run_failed` — which is why the ERRORED/run_failed checks
# below are real coverage for "exactly once", not decoration.
SCRIPT = json.dumps(
    [
        {"step": "emit_text", "text": REPLY},
        {"step": "finish", "reason": "end_turn"},
        {
            "step": "emit_request_input",
            "call_id": CALL,
            "kind": "choice",
            "title": f"{CARD} choose a target",
            "body": ["the daemon owns this list"],
            "options": [
                {"key": "alpha", "label": f"{PICK} alpha"},
                {"key": "beta", "label": "beta"},
            ],
        },
        {"step": "finish", "reason": "tool_use"},
        {"step": "expect_tool_result", "call_id": CALL},
        {"step": "emit_text", "text": RESUMED},
        {"step": "finish", "reason": "end_turn"},
    ],
    separators=(",", ":"),
)

profile = os.environ.get("LIVE_PROBE_PROFILE") or tempfile.mkdtemp(prefix="haider-live-probe-")
profile = probelib.require_throwaway_profile(profile)
os.makedirs(profile, exist_ok=True)
store = os.path.join(profile, "profile")
os.makedirs(store, exist_ok=True)

# `haider` resolves its sibling daemon and TUI payload, never PATH copies,
# so the probe stages the complete runtime bundle in its private directory.
bindir = os.path.join(profile, "bin")
os.makedirs(bindir, exist_ok=True)
shutil.copy2(haider, os.path.join(bindir, "haider"))
shutil.copy2(haiderd, os.path.join(bindir, "haiderd"))
shutil.copy2(haider_tui, os.path.join(bindir, "haider-tui"))
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
second_sink = [b""]
third_sink = [b""]
pid = None
fd = None
daemon_pids_after = []
journal = None

try:
    # The daemon also locks down its home-level state and resolves runtime
    # paths independently of HAIDER_PROFILE_DIR. Keep every root private,
    # matching the QA harness: no chmod/read of the invoking user's ~/.haider.
    probe_home = os.path.join(profile, "home")
    # The daemon's Unix socket lives under the runtime root; macOS TMPDIR
    # (/var/folders/.../T/, ~49 chars) plus the throwaway profile path pushes
    # that socket past the 104-byte sun_path limit, and an explicit
    # HAIDER_RUNTIME_DIR that exceeds it fails loudly (client profile.rs).
    # Keep the runtime root short and still throwaway.
    probe_runtime = tempfile.mkdtemp(prefix="haider-live-rt-", dir="/tmp")
    assert len(probe_runtime) < 60, f"runtime root too long for sun_path: {probe_runtime}"
    env_extra = {
        "HOME": probe_home,
        "USERPROFILE": probe_home,
        "XDG_CACHE_HOME": os.path.join(probe_home, ".cache"),
        "XDG_CONFIG_HOME": os.path.join(probe_home, ".config"),
        "XDG_DATA_HOME": os.path.join(probe_home, ".local", "share"),
        "XDG_STATE_HOME": os.path.join(probe_home, ".local", "state"),
        "XDG_RUNTIME_DIR": probe_runtime,
        "HAIDER_PROFILE_DIR": store,
        "HAIDER_RUNTIME_DIR": probe_runtime,
        "HAIDER_DISCOVERY_DISABLED": "1",
        "HAIDER_NO_UPDATE_CHECK": "1",
        "HAIDER_TEST_DEVICE_NAME": "test-mac",
        "HAIDER_TEST_FAKE_PROVIDER": SCRIPT,
    }
    for directory in (probe_home, probe_runtime):
        os.makedirs(directory, mode=0o700, exist_ok=True)

    inherited_environment = os.environ.copy()

    def live_environment():
        environment = inherited_environment.copy()
        for var in tuple(environment):
            if var.startswith("HAIDER_") or var.endswith(("_API_KEY", "_TOKEN", "_SECRET")):
                environment.pop(var, None)
        for var in ("NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "FORCE_COLOR", "COLORTERM"):
            environment.pop(var, None)
        environment["TERM"] = "xterm-256color"
        environment.update(env_extra)
        return environment

    def daemon_processes(stage):
        try:
            result = subprocess.run(
                ["pgrep", "-f", os.path.join(bindir, "haiderd")],
                capture_output=True, text=True, check=False,
            )
        except OSError as error:
            checks.append((f"{stage} process inspection unavailable: {error}", False))
            return [], False
        available = result.returncode in (0, 1)
        if not available:
            checks.append((
                f"{stage} pgrep failed: rc={result.returncode}, stderr={result.stderr.strip()}",
                False,
            ))
        return ([p for p in result.stdout.split() if p] if available else []), available

    def spawn_live(cols, rows, binary):
        """probelib.spawn, but for bare `haider` with the live env pinned."""
        import pty
        import struct
        import termios
        import fcntl

        child, child_fd = pty.fork()
        if child == 0:
            os.environ.clear()
            os.environ.update(live_environment())
            os.execv(binary, [binary])
        fcntl.ioctl(child_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(child, signal.SIGWINCH)
        return child, child_fd

    def boot(cols, rows):
        """A fresh live terminal, waited to its first alt-screen frame."""
        child, child_fd = spawn_live(cols, rows, probe_haider)
        child_sink = [b""]
        child_pump = probelib.make_pump(child_fd, child_sink)
        deadline = time.time() + 25
        while time.time() < deadline and b"\x1b[?1049h" not in child_sink[0]:
            child_pump(0.2)
        return child, child_fd, child_sink, child_pump

    def seen(child_sink, needle, start=0):
        return needle.encode() in child_sink[0][start:]

    def wait_for(child_pump, child_sink, needle, seconds, start=0):
        deadline = time.time() + seconds
        while time.time() < deadline and not seen(child_sink, needle, start):
            child_pump(0.2)
        return seen(child_sink, needle, start)

    def attach_row_one(child_fd, child_pump, child_sink, needle, start, tries=4):
        """Digit-1 attach from the live launcher, retried until the session
        surface PROVES it took (`needle` is content only that surface can
        paint).

        The roster arrives asynchronously over `session.list`, and the digit
        binding indexes the model's OWN session vector — a digit pressed
        before the list lands attaches nothing. There is no paintable
        readiness signal to wait on either: short frames SHED the launcher's
        session rows, so the row is invisible at 90x10 while the model
        already holds it. Hence retry, not a sleep. A digit that fell into
        the composer instead is erased first, because the same binding
        demands an EMPTY composer — one stray character would wedge every
        later attempt.
        """
        for _ in range(tries):
            if seen(child_sink, needle, start):
                return True
            write(child_fd, b"1")
            if wait_for(child_pump, child_sink, needle, 8.0, start):
                return True
            write(child_fd, b"\x7f")
            child_pump(0.3)
        return False

    def repaint(child_fd, child_pump, child_sink):
        """Force one FULL frame and return ONLY its bytes.

        ratatui paints DIFFS, so every sentinel that ever reached the screen
        stays in the stream forever — "the card is GONE" is simply not
        observable on the tail. A size change makes the backend clear and
        rewrite every cell, so the bytes after the second resize are the
        whole CURRENT screen. Each absence assertion below is paired with a
        presence assertion on the same bytes, so an empty or partial repaint
        can never pass as "gone".
        """
        probelib.set_size(child_fd, cols, rows - 1)
        child_pump(0.7)
        mark = len(child_sink[0])
        probelib.set_size(child_fd, cols, rows)
        child_pump(1.3)
        return child_sink[0][mark:]

    def read_journal():
        """Count automatic completion and forbidden menus in the daemon ledger.

        Scope by the planted call's session, not by menu coordinates: correct
        autonomous behavior creates no menu coordinate to count against. Read
        read-only while the daemon lives so its WAL remains available.
        """
        counts = {
            "menu_opened": 0,
            "menu_answered": None,
            "tool_result": 0,
            "run_failed": None,
            "resolutions": None,
            # DIAGNOSTIC, not an assertion. Every scoped count above reading
            # zero is consistent with two very different causes: the store the
            # probe opened contains none of this run's events, or it contains
            # them and no sentinel matched. Those need opposite fixes, and the
            # counts alone cannot tell them apart. `rows_read`, `decoded`, and
            # `blob_skipped_no_msgpack` discriminate in one CI run: zero rows
            # means the store is empty or unreadable; skipped blobs mean the
            # dependency is absent; decoded rows with no match mean journaling,
            # stream placement, or matcher shape needs inspection. Reported
            # unconditionally — a diagnostic behind an env var is a diagnostic
            # nobody has.
            "rows_read": 0,
            "decoded": 0,
            "blob_skipped_no_msgpack": 0,
        }
        connection = sqlite3.connect(
            "file:" + os.path.join(store, "store.sqlite") + "?mode=ro", uri=True
        )
        try:
            envelopes = []
            for (envelope,) in connection.execute("select envelope_json from events"):
                counts["rows_read"] += 1
                # v0.0.931+: new rows are msgpack BLOBs (bytes); legacy rows
                # stay JSON text. Decode both; without the msgpack module,
                # count blob rows honestly instead of crashing or lying.
                if isinstance(envelope, (bytes, bytearray)):
                    try:
                        import msgpack  # type: ignore

                        decoded = msgpack.unpackb(envelope, raw=False)
                    except ImportError:
                        counts["blob_skipped_no_msgpack"] += 1
                        continue
                else:
                    decoded = json.loads(envelope)
                envelopes.append(decoded)
            counts["decoded"] = len(envelopes)

            call_results = [
                envelope
                for envelope in envelopes
                if envelope.get("payload", {}).get("type") == "tool_result"
                and envelope["payload"].get("call_id") == CALL
            ]
            session_ids = {envelope.get("session_id") for envelope in call_results}
            run_coordinates = {
                (envelope.get("session_id"), envelope.get("run_id"))
                for envelope in call_results
                if envelope.get("run_id") is not None
            }
            if session_ids:
                scoped = [e for e in envelopes if e.get("session_id") in session_ids]
                counts["menu_opened"] = sum(
                    e.get("payload", {}).get("type") == "menu_opened" for e in scoped
                )
                counts["menu_answered"] = sum(
                    e.get("payload", {}).get("type") == "menu_answered" for e in scoped
                )
                counts["resolutions"] = sum(
                    session_id in session_ids
                    for session_id, in connection.execute(
                        "select session_id from menu_resolutions"
                    )
                )
            counts["tool_result"] = len(call_results)
            if run_coordinates:
                counts["run_failed"] = sum(
                    envelope.get("payload", {}).get("type") == "run_failed"
                    and (envelope.get("session_id"), envelope.get("run_id"))
                    in run_coordinates
                    for envelope in envelopes
                )
            if counts["blob_skipped_no_msgpack"]:
                print(
                    f"[probe] {counts['blob_skipped_no_msgpack']} "
                    "msgpack envelope rows skipped — "
                    "`pip install msgpack` for full coverage"
                )
        finally:
            connection.close()
        return counts

    pid, fd, sink, pump = boot(cols, rows)

    # 1. The launcher paints (the daemon was reachable, or was spawned).
    checks.append(("alt screen entered on the live front door", b"\x1b[?1049h" in sink[0]))

    # 2. A daemon is actually running for this profile.
    time.sleep(1.0)
    daemon_pids_after, _ = daemon_processes("startup")
    checks.append(("exactly one detached haiderd for the profile", len(daemon_pids_after) == 1))

    # 3. Type a prompt on the launcher and submit it. In live mode NOTHING
    #    appears until session.create answers — the row, the attach and the
    #    turn all follow the daemon.
    before_submit = len(sink[0])
    write(fd, b"live probe turn\r")

    # 4. The streamed reply must appear on a real frame.
    got_reply = wait_for(pump, sink, REPLY, 40, before_submit)
    checks.append(("a real provider reply reached a real frame", got_reply))

    # 5. …and it appeared on the SESSION surface, not the launcher. The
    #    composer's placeholder is surface-keyed (`PLACEHOLDER_SESSION` vs
    #    `PLACEHOLDER_LAUNCHER`, render.rs) and survives the 90-col squeeze,
    #    so this fails INDEPENDENTLY of the reply: a client that streamed the
    #    turn but never left the front door passes check 4 and fails here.
    #    (It replaces a check that OR-ed `got_reply` into itself and so could
    #    not fail at all — design review D1-3.)
    tail = sink[0][before_submit:]
    checks.append(
        ("the launcher gave way to the daemon's session surface", b"message haider" in tail)
    )

    # 5b. §6.4: "a second terminal attaches to the same session and sees
    #     contiguous live events". A SECOND `haider` process, on the same
    #     profile, must list the session the first one created and — on
    #     attaching — replay the same committed history.
    second_pid, second_fd, second_sink, second_pump = boot(cols, rows)
    checks.append(
        ("a second terminal reaches the live launcher", b"\x1b[?1049h" in second_sink[0])
    )
    checks.append(
        (
            "…and sees the SAME session's contiguous committed events",
            attach_row_one(second_fd, second_pump, second_sink, REPLY, 0),
        )
    )

    # 6. Autonomous request_input returns to the provider with no human key.
    #    The finite script makes duplicate continuation fail the run.
    before_request = len(sink[0])
    write(fd, b"resolve the request automatically\r")
    checks.append((
        "request_input automatically continued on the first terminal",
        wait_for(pump, sink, RESUMED, 40, before_request),
    ))
    checks.append((
        "the second terminal sees the same automatic continuation live",
        wait_for(second_pump, second_sink, RESUMED, 25),
    ))
    second_screen = repaint(second_fd, second_pump, second_sink)
    checks.append((
        "the live second terminal has no pending request_input card",
        RESUMED.encode() in second_screen
        and CARD.encode() not in second_sink[0]
        and PICK.encode() not in second_sink[0],
    ))

    # 6c. Retire the second terminal HERE (it has proved replay + live
    #     stream). R8 again: a client leaving never takes the daemon or the
    #     committed continuation with it — the cold terminal inherits history.
    for _ in range(3):
        try:
            os.write(second_fd, b"\x03")
        except OSError:
            break
        second_pump(0.5)
    checks.append(("the second terminal exits cleanly", probelib.reap(second_pid)))
    second_text = second_sink[0].decode("utf-8", "replace")
    checks.append(
        (
            "the second terminal never panicked",
            "panicked" not in second_text and "RUST_BACKTRACE" not in second_text,
        )
    )

    # 7. A cold attachment must reconstruct the completed automatic turn,
    #    not resurrect a pending request_input card from committed history.
    third_pid, third_fd, third_sink, third_pump = boot(cols, rows)
    checks.append(
        ("a cold third terminal reaches the live launcher", b"\x1b[?1049h" in third_sink[0])
    )
    checks.append((
        "the cold terminal reconstructs the automatic continuation from history",
        attach_row_one(third_fd, third_pump, third_sink, RESUMED, 0),
    ))

    # 8. Full repaints pair absence with continuation presence, so empty
    #    output cannot pass. Captured streams also reject a transient card.
    first_screen = repaint(fd, pump, sink)
    third_screen = repaint(third_fd, third_pump, third_sink)
    checks.append(
        (
            "the first terminal has no pending request_input card",
            RESUMED.encode() in first_screen
            and CARD.encode() not in sink[0]
            and PICK.encode() not in sink[0],
        )
    )
    checks.append(
        (
            "the cold terminal reconstructs no pending request_input card",
            RESUMED.encode() in third_screen
            and CARD.encode() not in third_sink[0]
            and PICK.encode() not in third_sink[0],
        )
    )
    # The PTY half of "exactly once": one resumption is painted, not two.
    # This is a real but PARTIAL witness — a second resumption that never
    # reached a frame would not show here, which is what the journal counts
    # in the `finally` block cover.
    checks.append(
        (
            "exactly one resumption on each final screen",
            first_screen.count(RESUMED.encode()) == 1
            and third_screen.count(RESUMED.encode()) == 1,
        )
    )
    # No run on either surface ever went ERRORED. The exhausted-script trap
    # is the point: a worker resumed a second time gets an empty segment,
    # the stream ends with no finish event, and the run fails — which the
    # status badge paints as `✗ ERRORED`.
    checks.append(
        (
            "no run ever painted ERRORED on any surface",
            b"ERRORED" not in sink[0]
            and b"ERRORED" not in second_sink[0]
            and b"ERRORED" not in third_sink[0],
        )
    )

    for _ in range(3):
        try:
            os.write(third_fd, b"\x03")
        except OSError:
            break
        third_pump(0.5)
    checks.append(("the cold terminal exits cleanly", probelib.reap(third_pid)))
    third_text = third_sink[0].decode("utf-8", "replace")
    checks.append(
        (
            "the cold terminal never panicked",
            "panicked" not in third_text and "RUST_BACKTRACE" not in third_text,
        )
    )

    # 10. Bonus (W3c3.1, review D1-2): `/voice` must NOT mint a card in live
    #     mode. It has no committed opening envelope, so the live loop could
    #     never close it — one press would block every later card, including
    #     any future daemon menu. This is independent of autonomous
    #     request_input policy: /voice is still demo-only. Live mode owes an honest
    #     flash instead, exactly as `/reset` does.
    before_voice = len(sink[0])
    write(fd, b"/voice\r")
    voice_flashed = wait_for(pump, sink, "demo only", 12, before_voice)
    checks.append(
        (
            "`/voice` flashes in live mode instead of minting an unclosable card",
            voice_flashed and not seen(sink, "enable duplex speech", before_voice),
        )
    )

    # 11. Quit cleanly: ⌃C from the session walks back to the launcher, a
    #     second quits. The child may already be gone (a dead TUI is caught
    #     by the reap check, not by an EIO traceback here).
    for _ in range(3):
        try:
            os.write(fd, b"\x03")
        except OSError:
            break
        pump(0.7)
finally:
    text = sink[0].decode("utf-8", "replace")
    # Read the ledger BEFORE reaping the daemon: a live daemon guarantees a
    # readable WAL for the read-only connection.
    try:
        journal = read_journal()
    except Exception as error:  # noqa: BLE001 — an unreadable ledger IS a failure
        print(f"live-probe: journal unreadable: {error}", file=sys.stderr)
        journal = None
    # The daemon must survive the client's exit (R8: closing a connection
    # never implies daemon shutdown) — then the probe reaps it.
    time.sleep(0.5)
    survivors, process_inspection_available = daemon_processes("shutdown")
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
    cleanup_ok = True
    if not process_inspection_available:
        # The advisory haiderd.pid contains only a PID, not profile identity.
        # Use the CLI's profile/Welcome + peer-credential authenticated stop
        # path instead of signalling an unverified (possibly recycled) PID.
        # 22.5s = daemon stop's existing 20s budget + probelib.reap's 2.5s
        # child-exit allowance. This is cleanup only, never an R8 witness.
        try:
            cleanup = subprocess.run(
                [probe_haider, "daemon", "stop", "--json", "--timeout", "20s"],
                env=live_environment(), capture_output=True, text=True,
                check=False, timeout=22.5,
            )
            cleanup_ok = cleanup.returncode == 0
            checks.append((
                f"authenticated private daemon cleanup: rc={cleanup.returncode}, "
                f"stderr={cleanup.stderr.strip()}",
                cleanup_ok,
            ))
        except (OSError, subprocess.TimeoutExpired) as error:
            cleanup_ok = False
            checks.append((f"authenticated private daemon cleanup failed: {error}", False))
    if not cleanup_ok:
        print(f"live-probe: preserving profile after failed cleanup: {profile}", file=sys.stderr)
    if cleanup_ok and not os.environ.get("LIVE_PROBE_PROFILE"):
        shutil.rmtree(profile, ignore_errors=True)
        shutil.rmtree(probe_runtime, ignore_errors=True)

# The half of "exactly once" no terminal can show: the daemon's ledger. Keep
# these as five separately reported facts. Menu facts are scoped to the
# planted call's session even when no menu exists; tool results to CALL and
# failures to its run. No inferred menu coordinate can make absence vacuous.
#
# Print how much the reader actually SAW before reporting what it matched.
# On v0.0.950 every scoped count read 0 or UNAVAILABLE, which is equally
# consistent with "the store held none of this run's events" and "the store was
# fine and no sentinel matched" — opposite causes needing opposite fixes. This
# line separates them without another guess: the store is WAL-mode and the probe
# opens it read-only while the daemon is still writing, so an empty read is a
# live hypothesis rather than a wild one.
for key, label, expected in (
    ("menu_opened", "sentinel-session menu_opened events", 0),
    ("menu_answered", "sentinel-session menu_answered events", 0),
    ("resolutions", "sentinel-session menu_resolutions rows", 0),
    ("tool_result", f"tool_result events for call_id {CALL}", 1),
    ("run_failed", "sentinel-run run_failed events", 0),
):
    value = journal[key] if journal is not None else None
    actual = value if value is not None else "UNAVAILABLE"
    checks.append(
        (
            f"journal {label}: EXPECTED {expected}, ACTUAL {actual}",
            value == expected,
        )
    )

# No secret may ever ride a live frame (the M3 sentinel leg's PTY half).
# The reader's OWN visibility, reported as a check so it lands inside the
# window ladder.sh actually shows. v0.0.951 printed this with a bare print()
# BEFORE the verdicts — it executed and was discarded, because ladder.sh tails
# only the last 25 lines of a failing probe (ladder.sh:73, :89). A diagnostic
# outside the visible window is a diagnostic nobody has, which is the exact
# defect this line exists to end.
#
# Always True: it reports, it does not gate. Zero rows means the store held
# none of this run's events; many decoded rows mean the store was readable and
# journaling, stream placement, or matcher shape needs inspection. Those need
# different fixes and the counts alone cannot separate them.
_rows = journal["rows_read"] if journal is not None else None
_decoded = journal["decoded"] if journal is not None else None
_blob_skipped = journal["blob_skipped_no_msgpack"] if journal is not None else None
checks.append(
    (
        f"[diagnostic] journal reader saw rows_read="
        f"{_rows if _rows is not None else 'UNAVAILABLE'}"
        f" decoded={_decoded if _decoded is not None else 'UNAVAILABLE'}"
        f" blob_skipped_no_msgpack="
        f"{_blob_skipped if _blob_skipped is not None else 'UNAVAILABLE'}"
        f" (0 rows => store empty/unreadable; decoded rows with no match => "
        "inspect journaling/stream/matcher)",
        True,
    )
)
checks.append(("no `sk-` key material in any frame", not re.search(rb"sk-[A-Za-z0-9_\-]{8,}", sink[0] + second_sink[0] + third_sink[0])))

child_clean = probelib.reap(pid) if pid is not None else False
if os.environ.get("LIVE_PROBE_DUMP"):
    sys.stderr.write(f"journal={journal}\n")
    for label, captured in (
        ("FIRST", sink[0]),
        ("SECOND", second_sink[0]),
        ("THIRD", third_sink[0]),
    ):
        sys.stderr.write(f"\n===== {label} =====\n")
        sys.stderr.write(captured.decode("utf-8", "replace"))
probelib.verdict("pty-probe-live", sink[0], child_clean, checks)
