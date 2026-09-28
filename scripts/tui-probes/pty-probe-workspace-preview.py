#!/usr/bin/env python3
"""Golden PTY probe for the fresh interactive dated-workspace preview."""

import json
import os
import pty
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
haider_tui = os.path.join(os.path.dirname(os.path.abspath(haider)), "haider-tui")
golden_path = os.path.join(here, "fixtures", "workspace-preview.120x30.golden")

for binary in (haider, haiderd, haider_tui):
    if not os.access(binary, os.X_OK):
        print(f"workspace-preview: missing binary {binary}", file=sys.stderr)
        sys.exit(2)

profile_root = probelib.require_throwaway_profile(
    tempfile.mkdtemp(prefix="haider-live-probe-workspace-")
)
home = os.path.realpath(os.path.join(profile_root, "home"))
documents = os.path.join(home, "Documents")
runtime = os.path.realpath(tempfile.mkdtemp(prefix="haider-live-rt-", dir="/tmp"))
bindir = os.path.join(profile_root, "bin")
for directory in (home, documents, runtime, bindir):
    os.makedirs(directory, mode=0o700, exist_ok=True)
    os.chmod(directory, 0o700)
for source in (haider, haiderd, haider_tui):
    shutil.copy2(source, os.path.join(bindir, os.path.basename(source)))
probe_haider = os.path.join(bindir, "haider")
probe_tui = os.path.join(bindir, "haider-tui")


def live_environment():
    environment = os.environ.copy()
    for name in tuple(environment):
        if name.startswith("HAIDER_") or name.endswith(("_API_KEY", "_TOKEN", "_SECRET")):
            environment.pop(name, None)
    for name in (
        "CI",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "FORCE_COLOR",
        "COLORTERM",
    ):
        environment.pop(name, None)
    environment.update(
        {
            "TERM": "xterm-256color",
            "HOME": home,
            "USERPROFILE": home,
            "XDG_CACHE_HOME": os.path.join(home, ".cache"),
            "XDG_CONFIG_HOME": os.path.join(home, ".config"),
            "XDG_DATA_HOME": os.path.join(home, ".local", "share"),
            "XDG_STATE_HOME": os.path.join(home, ".local", "state"),
            "XDG_RUNTIME_DIR": runtime,
            "HAIDER_RUNTIME_DIR": runtime,
            "HAIDER_DISCOVERY_DISABLED": "1",
            "HAIDER_NO_UPDATE_CHECK": "1",
            "HAIDER_TEST_DEVICE_NAME": "workspace-preview-probe",
            "HAIDER_TEST_FAKE_PROVIDER": json.dumps([]),
        }
    )
    return environment


sink = [b""]
checks = []
pid = None
fd = None
cleanup_ok = False
try:
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(home)
        os.environ.clear()
        os.environ.update(live_environment())
        os.execv(probe_tui, [probe_tui])
    probelib.set_size(fd, cols, rows)
    os.kill(pid, signal.SIGWINCH)
    pump = probelib.make_pump(fd, sink)
    deadline = time.time() + 30
    while time.time() < deadline:
        pump(0.2)
        if b"start a session" in probelib.plain(sink[0]) and b"created on first write" in probelib.plain(sink[0]):
            break

    probelib.set_size(fd, cols - 1, rows)
    pump(0.5)
    mark = len(sink[0])
    probelib.set_size(fd, cols, rows)
    pump(1.0)
    frame = sink[0][mark:]
    screen = "\n".join(probelib.screen_rows(frame).values())
    match = re.search(
        r"dir ~/Documents/Haider/(\d{4}-\d{2}-\d{2})/s-([0-9a-f]{32})"
        r" · created on first write",
        screen,
    )
    normalized = None
    if match:
        normalized = re.sub(r"\d{4}-\d{2}-\d{2}", "<hijri-date>", match.group(0))
        normalized = re.sub(r"s-[0-9a-f]{32}", "s-<allocation-id>", normalized)
    with open(golden_path, encoding="utf-8") as fixture:
        golden = fixture.read().strip()
    checks.extend(
        [
            ("owner-width frame matches the normalized workspace golden", normalized == golden),
            ("launcher does not report the launch cwd as its session dir", "dir ~ · mesh off" not in screen),
            (
                "only the permitted daily root exists before a session writes",
                match is not None
                and os.path.isdir(os.path.join(documents, "Haider", match.group(1)))
                and os.listdir(os.path.join(documents, "Haider", match.group(1))) == [],
            ),
        ]
    )
    for _ in range(3):
        try:
            os.write(fd, b"\x03")
        except OSError:
            break
        pump(0.4)
finally:
    child_clean = probelib.reap(pid) if pid is not None else False
    try:
        stopped = subprocess.run(
            [probe_haider, "daemon", "stop"],
            cwd=home,
            env=live_environment(),
            capture_output=True,
            text=True,
            check=False,
            timeout=20,
        )
        cleanup_ok = stopped.returncode == 0
        checks.append((f"private daemon cleanup rc={stopped.returncode}", cleanup_ok))
    except (OSError, subprocess.TimeoutExpired) as error:
        checks.append((f"private daemon cleanup failed: {error}", False))
    if cleanup_ok:
        shutil.rmtree(profile_root, ignore_errors=True)
        shutil.rmtree(runtime, ignore_errors=True)

probelib.verdict("pty-probe-workspace-preview", sink[0], child_clean, checks)
