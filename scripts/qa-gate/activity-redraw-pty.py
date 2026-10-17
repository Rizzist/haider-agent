#!/usr/bin/env python3
"""Assert real terminal cells after wide-glyph redraws; requires tmux and the fixture binary.

Build the terminal_redraw_probe example, then pass its absolute path and an
output directory. A private tmux server owns each probe's PTY; no other server
or application is touched. The full-application Fleet navigation is checked
separately by the lane's fake-provider integration harness.
"""
import argparse
import json
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results = []
    for width, height in [(118, 36), (80, 24)]:
        for index, glyph in enumerate(["⚙️", "✏️", "🖥️", "🔍", "界", "👩‍💻", "🇮🇷", "[run]", "é"]):
            name = f"haider-redraw-{time.time_ns()}"
            tmux = ["tmux", "-L", name]
            # exec argv via a shell-quoted command, since tmux runs a shell.
            import shlex
            command = shlex.join([str(args.binary.resolve()), glyph])
            try:
                subprocess.run(tmux + ["new-session", "-d", "-s", "probe", "-x", str(width), "-y", str(height), command], check=True)
                time.sleep(1.5)
                capture = subprocess.check_output(tmux + ["capture-pane", "-p", "-t", "probe"], text=True)
                (args.output / f"{width}-{index}.txt").write_text(capture)
                lines = capture.splitlines()
                passed = len(lines) == height and lines[3] == "row" and all(not line for i, line in enumerate(lines) if i != 3)
                results.append(dict(width=width, height=height, glyph=glyph, passed=passed, row=lines[3]))
            finally:
                subprocess.run(tmux + ["kill-server"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    (args.output / "results.json").write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
    assert all(row["passed"] for row in results), results


if __name__ == "__main__":
    main()
