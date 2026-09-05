#!/usr/bin/env python3
"""Compare frozen baseline/candidate binaries with existing turn-wall proof pins.

Runs four complete suites in A-B-B-A order. Each warm suite retains 25 samples
per shape after five warmups; each one-shot suite uses the existing lifecycle
proof defaults. A comparison passes when B's median is no higher than A's
median plus max(A MAD, B MAD). All original load/correctness pins must pass.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from turn_wall_harness import LOAD_LIMIT, run_harness, run_one_shot_harness
from turnperf_support import load_one_minute, median_mad, sha256_file


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    samples: dict[str, dict[str, list[float]]] = {
        label: {shape: [] for shape in ("warm_single", "warm_tool", "one_shot")}
        for label in ("A", "B")
    }
    bins = {"A": args.baseline.resolve(), "B": args.candidate.resolve()}
    runs = []
    for index, label in enumerate("ABBA", start=1):
        current_load = load_one_minute()
        if current_load >= LOAD_LIMIT:
            failure = {"status": "ENVIRONMENT-BLOCKED", "before_suite": index,
                       "load_1m": current_load, "required_below": LOAD_LIMIT,
                       "completed_runs": runs}
            (args.output_dir / "abba.json").write_text(json.dumps(failure, indent=2) + "\n")
            print(json.dumps(failure), flush=True)
            return 75
        for kind, runner in (("warm", run_harness), ("one_shot", run_one_shot_harness)):
            print(f"{index}/4 {label} {kind}: starting existing proof harness", flush=True)
            report = runner(bins[label], commit_label=f"economydiet-{label}-{index}")
            output = args.output_dir / f"{index}-{label}-{kind}.json"
            output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
            runs.append({"label": label, "kind": kind, "path": output.name,
                         "sha256": sha256_file(output), "passed": report["passed"],
                         "measurement_accepted": report["measurement_accepted"]})
            if not report["passed"] or not report["measurement_accepted"]:
                environment_only = not report["measurement_accepted"] and not report["failures"]
                failure = {"status": "ENVIRONMENT-BLOCKED" if environment_only else "NO_SHIP", "completed_runs": runs,
                           "failures": report["failures"],
                           "measurement_reasons": report["measurement_reasons"]}
                (args.output_dir / "abba.json").write_text(json.dumps(failure, indent=2) + "\n")
                print(json.dumps(failure), flush=True)
                return 75 if environment_only else 1
            if kind == "warm":
                for shape in ("single", "tool"):
                    samples[label][f"warm_{shape}"].extend(
                        row["wall_ms"] for row in report["samples"][shape])
            else:
                samples[label][kind].extend(row["wall_ms"] for row in report["samples"])
            print(f"{index}/4 {label} {kind}: proof accepted", flush=True)
    comparison = {}
    for shape in samples["A"]:
        before, before_mad = median_mad(samples["A"][shape])
        after, after_mad = median_mad(samples["B"][shape])
        tolerance = max(before_mad, after_mad)
        comparison[shape] = {
            "baseline_count": len(samples["A"][shape]),
            "candidate_count": len(samples["B"][shape]),
            "baseline_median_ms": before, "baseline_mad_ms": before_mad,
            "candidate_median_ms": after, "candidate_mad_ms": after_mad,
            "delta_ms": after - before, "max_mad_ms": tolerance,
            "neutral_within_mad": after <= before + tolerance,
        }
    passed = all(row["neutral_within_mad"] for row in comparison.values())
    report = {
        "schema": "haider.economydiet.abba.v1", "order": "ABBA",
        "status": "SHIP" if passed else "NO_SHIP",
        "criterion": "B median <= A median + max(A MAD, B MAD); existing load and correctness proof pins unchanged",
        "binaries": {label: {name: sha256_file(path / name) for name in ("haider", "haiderd")}
                     for label, path in bins.items()},
        "runs": runs, "comparison": comparison,
    }
    (args.output_dir / "abba.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, sort_keys=True), flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
