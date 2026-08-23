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
             -> a REAL daemon `request_input` card opens
             -> a THIRD, COLD terminal reconstructs that pending card from
                committed history and ANSWERS it
             -> the one worker resumes; both surfaces show the continuation
                and neither still shows the card
             -> quit

That last leg is §6.4's "menu answers from either control attachment
resume the one worker exactly once" row. It used to be backed by
`checks.append((..., True))` — a literal, which is not a gate (design
review D1-3). The card is now the daemon's own: `FakeStep::EmitRequestInput`
(JSON `emit_request_input`) makes the actor allocate and journal a protocol
menu, so every sentinel below travels provider -> actor -> journal -> RPC
-> projection -> frame. Nothing in this file can fabricate one.

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

for binary in (haider, haiderd):
    if not os.access(binary, os.X_OK):
        print(f"live-probe: missing binary {binary}", file=sys.stderr)
        sys.exit(2)

# Four sentinels, each planted at a different point of the daemon's own
# pipeline so a check can name WHICH hop broke:
#   REPLY   — a streamed agent message (provider -> item journal -> frame)
#   CARD    — the `request_input` menu TITLE (provider -> actor-allocated
#             `MenuOpened` -> frame). Under height pressure the title is the
#             second row a card sheds, so it is asserted with…
#   PICK    — …an OPTION LABEL, which `menu_block` NEVER sheds. A local
#             `/voice`-style card could never carry it: the labels are the
#             daemon's, verbatim from the journal.
#   RESUMED — text from the segment the provider only reaches once the
#             ANSWER came back as a tool result. Seeing it is the proof the
#             one worker resumed.
REPLY = "LIVEPROBEREPLY"
CARD = "LIVEPROBECARD"
PICK = "LIVEPROBEPICK"
RESUMED = "LIVEPROBERESUMED"
CALL = "live-probe-input-1"

# Three FakeProvider SEGMENTS (`next_segment` cuts at every terminal step),
# so the probe — not a sleep — decides when each hop happens:
#   1. turn one: one text item, clean finish.
#   2. turn two: the `request_input` call, finished with `tool_use`. The
#      actor parks the run in `InputRequired` and journals `MenuOpened`.
#   3. the RESUMPTION, gated by `expect_tool_result`: the fake provider
#      REFUSES this segment (typed Internal error) unless the request
#      actually carries the answer for `CALL`, so RESUMED on screen can
#      only mean the answer completed the round trip.
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
second_sink = [b""]
third_sink = [b""]
pid = None
fd = None
daemon_pids_after = []
journal = None

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
        """The daemon's own ledger for the one thing the PTY cannot show:
        HOW MANY times the menu resolved. `menu_resolutions` is the CAS
        table the answer path writes through (one row per menu, by primary
        key); the event counts come from the committed envelopes. The planted
        card identifies this probe's menu, session and run so unrelated
        profile activity cannot change these counts. Read read-only, while the
        daemon still lives, so its WAL is readable."""
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
            # counts alone cannot tell them apart. `rows_read` discriminates in
            # one CI run: zero means the store is empty or unreadable to this
            # reader; thousands means the store is fine and the matchers are
            # wrong. Reported unconditionally — a diagnostic behind an env var
            # is a diagnostic nobody has.
            "rows_read": 0,
            "decoded": 0,
        }
        connection = sqlite3.connect(
            "file:" + os.path.join(store, "store.sqlite") + "?mode=ro", uri=True
        )
        try:
            blob_rows = 0
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
                        blob_rows += 1
                        continue
                else:
                    decoded = json.loads(envelope)
                envelopes.append(decoded)
            counts["decoded"] = len(envelopes)

            def is_sentinel_menu(payload):
                options = payload.get("options", [])
                return (
                    payload.get("type") == "menu_opened"
                    and payload.get("kind") == "choice"
                    and payload.get("title") == f"{CARD} choose a target"
                    and payload.get("body") == ["the daemon owns this list"]
                    and payload.get("origin") == "request_input"
                    and any(
                        option.get("key") == "alpha"
                        and option.get("label") == f"{PICK} alpha"
                        for option in options
                    )
                )

            openings = [
                envelope
                for envelope in envelopes
                if is_sentinel_menu(envelope.get("payload", {}))
            ]
            menu_coordinates = {
                (envelope.get("session_id"), envelope["payload"].get("id"))
                for envelope in openings
            }
            call_results = [
                envelope
                for envelope in envelopes
                if envelope.get("payload", {}).get("type") == "tool_result"
                and envelope["payload"].get("call_id") == CALL
            ]
            run_coordinates = {
                (envelope.get("session_id"), envelope.get("run_id"))
                for envelope in openings + call_results
                if envelope.get("run_id") is not None
            }

            counts["menu_opened"] = len(openings)
            if menu_coordinates:
                counts["menu_answered"] = sum(
                    envelope.get("payload", {}).get("type") == "menu_answered"
                    and (
                        envelope.get("session_id"),
                        envelope["payload"].get("menu"),
                    )
                    in menu_coordinates
                    for envelope in envelopes
                )
                counts["resolutions"] = sum(
                    (session_id, menu_id) in menu_coordinates
                    for session_id, menu_id in connection.execute(
                        "select session_id, menu_id from menu_resolutions"
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
            if blob_rows:
                print(
                    f"[probe] {blob_rows} msgpack envelope rows skipped — "
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

    # 6. THE DAEMON'S OWN CARD. A second turn drives script segment 2:
    #    `emit_request_input` -> the actor allocates the menu, journals
    #    `MenuOpened`, and parks the run in `InputRequired`. Both the title
    #    and an option label are asserted: the title is what a reader looks
    #    for, the option label is what a LOCAL card could never invent (and
    #    what the card never sheds under height pressure). If this goes
    #    quiet, the answer legs below have nothing to answer — read
    #    `daemon.log` before suspecting the TUI.
    before_card = len(sink[0])
    write(fd, b"open the card\r")
    card_on_first = wait_for(pump, sink, PICK, 40, before_card) and seen(
        sink, CARD, before_card
    )
    checks.append(("a REAL daemon request_input card opened on a real frame", card_on_first))

    # 6b. The second terminal is still attached and still on the session
    #     surface: it must paint the SAME card from the live stream, with no
    #     action of its own. A card only the submitting terminal can see is
    #     not a shared session.
    checks.append(
        (
            "…and the second terminal sees that same card on the live stream",
            wait_for(second_pump, second_sink, PICK, 25),
        )
    )

    # 6c. Retire the second terminal HERE (it has proved replay + live
    #     stream). R8 again: a client leaving never takes the daemon or the
    #     parked menu with it — the cold terminal below inherits both.
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

    # 7. §6.4's "preserves/reconstructs a pending `request_input`": a THIRD,
    #    COLD terminal — started after the card opened, so it never saw the
    #    opening event live — attaches and must rebuild the parked card out
    #    of committed history alone.
    third_pid, third_fd, third_sink, third_pump = boot(cols, rows)
    checks.append(
        ("a cold third terminal reaches the live launcher", b"\x1b[?1049h" in third_sink[0])
    )
    cold_card = attach_row_one(third_fd, third_pump, third_sink, PICK, 0) and seen(
        third_sink, CARD
    )
    checks.append(
        ("…and RECONSTRUCTS the pending card from committed history alone", cold_card)
    )

    # 8. §6.4's headline row: THE ANSWER COMES FROM THE OTHER CONTROL
    #    ATTACHMENT. The terminal that submitted the turn never touches the
    #    card; the cold one answers it with digit 1. That only works if this
    #    connection recorded the menu's COMMITTED coordinates (request_seq +
    #    worker_generation) while replaying, because `LiveDriver` refuses to
    #    send an answer it has no coordinates for — the exact silent-drop
    #    that made `/voice` cards unclosable in live mode.
    before_answer_third = len(third_sink[0])
    before_answer_first = len(sink[0])
    write(third_fd, b"1")
    deadline = time.time() + 40
    while time.time() < deadline and not (
        seen(third_sink, RESUMED, before_answer_third)
        and seen(sink, RESUMED, before_answer_first)
    ):
        third_pump(0.2)
        pump(0.2)
    checks.append(
        (
            "answering from the OTHER attachment resumed the one worker",
            seen(third_sink, RESUMED, before_answer_third),
        )
    )
    # The resumption is not the answering client's private event: the
    # continuation is journaled, so the terminal that OWNS the turn shows it
    # too. (`expect_tool_result` guards the other half — the provider only
    # emits RESUMED at all if the answer arrived as the tool result for CALL.)
    checks.append(
        (
            "…and the committed continuation reached the FIRST terminal too",
            seen(sink, RESUMED, before_answer_first),
        )
    )

    # 9. AND THE CARD IS GONE — on both. A card that took an answer but
    #    never closed is precisely the bug class this lane fixed, and it is
    #    invisible on a diff-painting stream, so each surface is forced to
    #    repaint in full. The presence half (RESUMED) makes the absence half
    #    non-vacuous: an empty repaint fails instead of passing.
    first_screen = repaint(fd, pump, sink)
    third_screen = repaint(third_fd, third_pump, third_sink)
    checks.append(
        (
            "the answered card is gone from the first terminal's full screen",
            RESUMED.encode() in first_screen
            and CARD.encode() not in first_screen
            and PICK.encode() not in first_screen,
        )
    )
    checks.append(
        (
            "…and gone from the answering terminal's full screen",
            RESUMED.encode() in third_screen
            and CARD.encode() not in third_screen
            and PICK.encode() not in third_screen,
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
    checks.append(("the answering terminal exits cleanly", probelib.reap(third_pid)))
    third_text = third_sink[0].decode("utf-8", "replace")
    checks.append(
        (
            "the answering terminal never panicked",
            "panicked" not in third_text and "RUST_BACKTRACE" not in third_text,
        )
    )

    # 10. Bonus (W3c3.1, review D1-2): `/voice` must NOT mint a card in live
    #     mode. It has no committed opening envelope, so the live loop could
    #     never close it — one press would block every later card, including
    #     the daemon's own `request_input` above. Live mode owes an honest
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

# The half of "exactly once" no terminal can show: the daemon's ledger. Keep
# these as five separately reported facts: a bare composite False hid which
# behavior was wrong for 25 releases. Menu facts are scoped to the planted
# card's (session, menu), failures to its (session, run), and ToolResult to the
# planted call id. `menu_resolutions` can hold only one row for that coordinate
# because (session_id, menu_id) is its primary key.
#
# Print how much the reader actually SAW before reporting what it matched.
# On v0.0.950 every scoped count read 0 or UNAVAILABLE, which is equally
# consistent with "the store held none of this run's events" and "the store was
# fine and no sentinel matched" — opposite causes needing opposite fixes. This
# line separates them without another guess: the store is WAL-mode and the probe
# opens it read-only while the daemon is still writing, so an empty read is a
# live hypothesis rather than a wild one.
_rows = journal["rows_read"] if journal is not None else None
_decoded = journal["decoded"] if journal is not None else None
print(
    f"  [diagnostic] journal reader: rows_read="
    f"{_rows if _rows is not None else 'UNAVAILABLE'}"
    f" decoded={_decoded if _decoded is not None else 'UNAVAILABLE'}"
    f"  (0 rows => store empty or unreadable to this reader;"
    f" many rows => matchers are wrong)"
)

for key, label, expected in (
    ("menu_opened", "sentinel menu_opened events", 1),
    ("menu_answered", "sentinel menu_answered events", 1),
    ("resolutions", "sentinel menu_resolutions rows", 1),
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
checks.append(("no `sk-` key material in any frame", not re.search(r"sk-[A-Za-z0-9_\-]{8,}", text)))

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
