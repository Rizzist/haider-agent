#!/usr/bin/env python3
"""Run the M1 client and record its kernel-maintained lifetime maximum RSS."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import resource
import subprocess


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("a client command is required after --")

    completed = subprocess.run(command, check=False)
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    args.output.write_text(
        json.dumps({"max_rss_bytes": int(usage.ru_maxrss)}) + "\n",
        encoding="utf-8",
    )
    return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
