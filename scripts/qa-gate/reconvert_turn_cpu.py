#!/usr/bin/env python3
"""Offline reconversion of the known legacy Darwin warm-turn CPU samples."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import statistics

from turnperf_support import ProofError


# Only this audited sampler is known to have generated the 971 saved samples.
# Refuse unknown methods and corrected reports instead of silently scaling twice.
LEGACY_SUPPORT_SHA256 = "39ac4b24c503283e09dcd003126230075a58a5077a24ad504d3b2f7deffd6970"


def summary(values: list[float]) -> dict[str, float]:
    median = statistics.median(values)
    return {"median": median, "mad": statistics.median(abs(x - median) for x in values),
            "total": sum(values)}


def reconvert(document: dict, numer: int, denom: int) -> dict:
    if numer <= 0 or denom <= 0:
        raise ProofError("historical host timebase must be positive")
    if (document.get("schema") != "haider.turn-wall.v1"
            or document.get("mode") == "one-shot"
            or "cpu_accounting" in document
            or not document.get("host", {}).get("platform", "").startswith("macOS-")
            or document.get("binaries", {}).get("proxy_source_sha256") != LEGACY_SUPPORT_SHA256):
        raise ProofError("not an audited legacy Darwin warm report; refusing reconversion")
    shapes = {}
    for shape in ("single", "tool"):
        rows = document.get("samples", {}).get(shape, [])
        if not rows:
            raise ProofError(f"no saved {shape} samples")
        corrected = []
        for row in rows:
            for name in ("client_cpu_ms", "daemon_cpu_ms", "combined_cpu_ms"):
                if not math.isfinite(row[name]) or row[name] < 0:
                    raise ProofError(f"invalid saved {name}")
            if not math.isclose(row["combined_cpu_ms"], row["client_cpu_ms"] + row["daemon_cpu_ms"]):
                raise ProofError("legacy combined CPU is not client + daemon self")
            # The saved daemon delta is ticks / 1e6, not raw integer ticks.
            # Scale just that component; preserve its original float precision.
            daemon = row["daemon_cpu_ms"] * numer / denom
            sampled = row.get("client_sampled_cpu_lower_bound_ms")
            corrected.append({
                "index": row["index"], "case_id": row["case_id"],
                "client_cpu_ms": row["client_cpu_ms"],
                "daemon_self_cpu_ms": daemon,
                "client_plus_daemon_self_cpu_ms": row["client_cpu_ms"] + daemon,
                "client_sampled_cpu_lower_bound_ms": None if sampled is None else sampled * numer / denom,
                "daemon_reaped_children_cpu_ms": None,
            })
        shapes[shape] = {
            "n": len(rows),
            "old_combined_cpu_ms": summary([row["combined_cpu_ms"] for row in rows]),
            **{key: summary([row[key] for row in corrected]) for key in (
                "client_cpu_ms", "daemon_self_cpu_ms", "client_plus_daemon_self_cpu_ms")},
            "samples": corrected,
        }
    return {"original_commit": document.get("commit"), "original_host": document["host"],
            "shapes": shapes}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inputs", type=Path, nargs="+")
    parser.add_argument("--timebase", type=int, nargs=2, required=True, metavar=("NUMER", "DENOM"),
                        help="timebase of the host that collected the SAVED samples (not necessarily this host)")
    args = parser.parse_args()
    result = {
        "method": "OFFLINE reconversion of existing samples; NOT fresh measurement",
        "timebase": dict(zip(("numer", "denom"), args.timebase)),
        "timebase_provenance": "explicit historical-host ratio supplied by caller",
        "formula": "client_cpu_ms + daemon_cpu_ms * numer / denom; never scale the getrusage client CPU",
        "exclusions": "daemon-reaped child CPU was not saved and cannot be recovered; later daemon work and provider CPU excluded",
        "boundary": "before CLI command to after CLI exit, before wait_session_idle",
        "reports": {},
    }
    try:
        for path in args.inputs:
            raw = path.read_bytes()
            result["reports"][str(path.resolve())] = {
                "sha256": hashlib.sha256(raw).hexdigest(),
                **reconvert(json.loads(raw), *args.timebase),
            }
    except (OSError, ValueError, KeyError, TypeError, ProofError) as error:
        parser.exit(1, f"offline CPU reconversion failed: {error}\n")
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
