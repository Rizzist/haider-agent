#!/usr/bin/env python3
"""Thin-client dependency proof and fixed ABBA measurement orchestration.

The turn harness remains the wall/JSONL/provider/lifecycle/RSS authority. The
read-only peer conformance CLI remains the conformance authority. Only the
--version floor needs a new measurement: direct-child wait4, with no daemon or
payload descendant. No load limit, sample count, or correctness pin is relaxed.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any

from gate.contract import VERSION_QUERY, budget_seconds
from turn_wall_harness import (
    LOAD_LIMIT, MEASURED_PER_SHAPE, WARMUPS_PER_SHAPE,
    run_harness, run_one_shot_harness,
)
from turnperf_support import ProofError, load_one_minute, median_mad, sha256_file

ROOT = Path(__file__).resolve().parents[2]
ORDER = "ABBA"
FORBIDDEN = frozenset({
    "haider-tui", "haider-stt", "haider-core", "haider-store", "haider-tools",
    "image", "ratatui", "ratatui-image", "cpal", "rodio", "arboard", "crossterm",
    "termbg", "alsa", "alsa-sys", "coreaudio-rs", "coreaudio-sys", "rubato",
})
FORBIDDEN_PREFIXES = ("symphonia", "dasp", "audiopus")
# Namespace/source markers identify actual linked code. A literal sibling name
# like "haider-tui" is expected launcher data and is deliberately not evidence.
CODE_MARKERS = re.compile(
    rb"haider_(?:tui|stt|core|store|tools)(?:::|[0-9])"
    rb"|haider-(?:tui|stt|core|store|tools)[/\\]src[/\\]"
    rb"|(?:ratatui|cpal)::|image::(?:codecs|imageops|images)::"
    rb"|(?:image|ratatui|cpal)-[0-9][^\x00\r\n ]*[/\\]src[/\\]"
)
GRAPHICS_LIBRARIES = re.compile(
    r"(?:CoreGraphics|CoreAudio|AudioToolbox|AVFoundation|libasound|libwayland|libX11|libvulkan|opengl32)",
    re.IGNORECASE,
)


class EnvironmentBlocked(RuntimeError):
    pass


def write_json(path: Path, document: Any) -> None:
    path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def dependency_names(tree: str) -> set[str]:
    names = set()
    for line in tree.splitlines():
        match = re.match(r"^([A-Za-z0-9_-]+) v[^ ]+", line.strip())
        if match:
            names.add(match.group(1))
    if "haider-cli" not in names:
        raise ProofError("cargo tree did not contain the haider-cli root")
    return names


def forbidden_dependencies(names: set[str]) -> list[str]:
    return sorted(name for name in names if name in FORBIDDEN or name.startswith(FORBIDDEN_PREFIXES))


def executable(bin_dir: Path, name: str) -> Path:
    path = bin_dir / (name + (".exe" if os.name == "nt" else ""))
    if not path.is_file():
        raise ProofError(f"required executable is absent: {path}")
    return path


def inventory(bin_dir: Path, *, payload_required: bool) -> dict[str, Any]:
    result = {}
    for name in ("haider", "haiderd", "haider-tui"):
        path = bin_dir / (name + (".exe" if os.name == "nt" else ""))
        if name == "haider-tui" and not payload_required and not path.is_file():
            result[name] = None
            continue
        path = executable(bin_dir, name)
        result[name] = {"path": str(path.resolve()), "bytes": path.stat().st_size,
                        "sha256": sha256_file(path)}
    if result["haiderd"]["bytes"] <= 10 * 1024 * 1024:
        raise ProofError("haiderd must exceed 10 MiB (registry #64)")
    return result


def assert_frozen(binaries: dict[str, Any]) -> None:
    for label, members in binaries.items():
        for name, member in members.items():
            if member and sha256_file(Path(member["path"])) != member["sha256"]:
                raise ProofError(f"frozen artifact changed during measurement: {label}/{name}")


def boundary_report(bin_dir: Path, *, target: str = "all") -> dict[str, Any]:
    command = ["cargo", "tree", "--locked", "--offline", "-p", "haider-cli",
               "--edges", "normal", "--target", target, "--prefix", "none", "--format", "{p}"]
    graph = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
    if graph.returncode:
        raise ProofError(f"normal dependency graph unavailable: {graph.stderr.strip()}")
    names = dependency_names(graph.stdout)
    denied = forbidden_dependencies(names)
    path = executable(bin_dir, "haider")
    code_hits = sorted({match.group().decode("utf-8", errors="replace")
                        for match in CODE_MARKERS.finditer(path.read_bytes())})
    inspections = []
    for program, options in (("llvm-nm", ["--demangle"]), ("nm", ["-a"]),
                             ("otool", ["-L"]), ("llvm-readobj", ["--needed-libs"]),
                             ("objdump", ["-p"])):
        tool = shutil.which(program)
        if not tool:
            continue
        result = subprocess.run([tool, *options, str(path)], capture_output=True,
                                text=True, errors="replace", check=False)
        code_hits.extend(match.group().decode("utf-8", errors="replace")
                         for match in CODE_MARKERS.finditer(result.stdout.encode()))
        libraries = sorted(set(GRAPHICS_LIBRARIES.findall(result.stdout)))
        inspections.append({"tool": program, "exit_code": result.returncode,
                            "forbidden_libraries": libraries,
                            "stderr": result.stderr.strip()[:1000],
                            "output": result.stdout if program not in {"nm", "llvm-nm"} else None,
                            "symbol_output_present": bool(result.stdout.strip())})
    failures = ([f"normal dependency present: {name}" for name in denied]
                + [f"linked code marker: {hit}" for hit in sorted(set(code_hits))]
                + [f"linked graphics/audio library: {name}" for item in inspections
                   for name in item["forbidden_libraries"]])
    return {"schema": "haider.thinexe.boundary.v1", "passed": not failures,
            "graph_command": command, "normal_dependencies": sorted(names),
            "graph": graph.stdout, "failures": failures,
            "artifact": {"path": str(path), "bytes": path.stat().st_size,
                         "sha256": sha256_file(path)}, "inspections": inspections,
            "limitations": "Stripped symbols may be absent; normal-edge dependency exclusion is the structural proof. Raw binary/source markers and available native tools independently inspect the built artifact. Dev/build edges are intentionally excluded."}


def require_quiet_host() -> float:
    load = load_one_minute()
    if not math.isfinite(load) or load >= LOAD_LIMIT:
        raise EnvironmentBlocked(f"one-minute load {load} must be below {LOAD_LIMIT}")
    try:
        processes = subprocess.run(["ps", "-axo", "comm="], capture_output=True, text=True, check=False)
    except PermissionError as error:
        raise EnvironmentBlocked(
            f"process inventory denied; cannot prove absence of competing builds: {error}"
        ) from error
    if processes.returncode:
        raise EnvironmentBlocked("cannot establish whether a competing build is running")
    builds = sorted({Path(line.strip()).name for line in processes.stdout.splitlines()
                     if Path(line.strip()).name in {"cargo", "rustc", "rust-lld", "ld", "clang", "clang++"}})
    if builds:
        raise EnvironmentBlocked("competing build processes: " + ", ".join(builds))
    return load


def version_sample(path: Path, scratch: Path) -> dict[str, Any]:
    if not hasattr(os, "wait4"):
        raise EnvironmentBlocked("exact exec-floor CPU/peak RSS requires POSIX wait4")
    env = os.environ.copy()
    env.update(HOME=str(scratch), USERPROFILE=str(scratch), HAIDER_PROFILE_DIR=str(scratch / "profile"),
               HAIDER_RUNTIME_DIR=str(scratch / "runtime"), HAIDER_DISCOVERY_DISABLED="1",
               HAIDER_NO_UPDATE_CHECK="1")
    env.pop("HAIDER_CLIENT_FOOTPRINT_HOLD_MS", None)
    # Reuse the existing version-query deadline; the observer is only a kill
    # watchdog, not the timestamp authority. wait4 owns exact exit resources.
    timeout = budget_seconds(VERSION_QUERY)
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        timed_out = threading.Event()
        started = time.monotonic_ns()
        child = subprocess.Popen([str(path), "--version"], stdout=stdout, stderr=stderr,
                                 cwd=scratch, env=env, start_new_session=True)
        def kill() -> None:
            timed_out.set()
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        watchdog = threading.Timer(timeout, kill)
        watchdog.daemon = True
        watchdog.start()
        try:
            _pid, status, usage = os.wait4(child.pid, 0)
            ended = time.monotonic_ns()
            child.returncode = os.waitstatus_to_exitcode(status)
        finally:
            watchdog.cancel()
            watchdog.join()
        stdout.seek(0)
        stderr.seek(0)
        output = stdout.read().decode("utf-8", errors="replace")
        error = stderr.read().decode("utf-8", errors="replace")
    if timed_out.is_set() or child.returncode or not re.fullmatch(r"haider \S+\n", output):
        raise ProofError(f"--version failed: exit={child.returncode} stdout={output!r} stderr={error!r}")
    return {"wall_ms": (ended - started) / 1_000_000,
            "client_cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000,
            "client_peak_rss_kib": usage.ru_maxrss / 1024 if sys.platform == "darwin" else usage.ru_maxrss,
            "version": output.strip()}


def exec_floor(bin_dir: Path) -> dict[str, Any]:
    samples, warmups, loads = [], [], []
    with tempfile.TemporaryDirectory(prefix="thinexe-version-") as directory:
        for index in range(WARMUPS_PER_SHAPE + MEASURED_PER_SHAPE):
            loads.append(require_quiet_host())
            row = version_sample(executable(bin_dir, "haider"), Path(directory))
            (warmups if index < WARMUPS_PER_SHAPE else samples).append(row)
            loads.append(load_one_minute())
    accepted = all(math.isfinite(value) and value < LOAD_LIMIT for value in loads)
    return {"schema": "haider.thinexe.exec-floor.v1", "samples": samples,
            "warmups": warmups, "load_one_minute": loads, "passed": accepted,
            "measurement_accepted": accepted,
            "measurement_reasons": [] if accepted else ["load exceeded unchanged proof pin"],
            "failures": [], "authority": "Popen to wait4; --version has no descendants; exact own CPU and kernel peak RSS"}


def conformance_command(peer: Path, binary: Path, output: Path) -> list[str]:
    return [sys.executable, "-m", "bench.conformance", "--adapter", "haider-agent",
            "--executable", str(binary), "--model", "deepseek-v4-flash",
            "--context-window", "131072", "--max-output-tokens", "8192", "--max-turns", "20",
            "--process-timeout", "15", "--proxy-timeout", "20", "--round-index", "0",
            "--json-report", str(output)]


def run_conformance(peer: Path, bin_dir: Path, output: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    command = conformance_command(peer, executable(bin_dir, "haider"), output)
    loads: list[float | None] = [require_quiet_host()]
    done = threading.Event()
    def sample_load() -> None:
        while not done.wait(1):
            try:
                loads.append(load_one_minute())
            except ProofError:
                loads.append(None)
    sampler = threading.Thread(target=sample_load, daemon=True)
    sampler.start()
    try:
        with output.with_suffix(".log").open("w") as log:
            result = subprocess.run(command, cwd=peer,
                                    env=os.environ | {"PYTHONDONTWRITEBYTECODE": "1"},
                                    stdout=log, stderr=subprocess.STDOUT, check=False)
    finally:
        done.set()
        sampler.join()
    loads.append(load_one_minute())
    if not output.is_file():
        raise ProofError(f"conformance produced no JSON report; see {output.with_suffix('.log')}")
    report = json.loads(output.read_text())
    accepted_load = all(value is not None and math.isfinite(value) and value < LOAD_LIMIT for value in loads)
    proof = {"command": command, "cwd": str(peer), "exit_code": result.returncode,
             "load_one_minute": loads, "strict_load_accepted": accepted_load,
             "measurement_accepted": bool(report.get("measurement", {}).get("accepted")) and accepted_load,
             "passed": report.get("overall") == "PASS" and result.returncode == 0,
             "raw_statuses": {case["name"]: case["status"] for case in report.get("cases", [])}}
    return report, proof


def require_client_rows(kind: str, report: dict[str, Any]) -> None:
    """Absent native client counters are unavailable evidence, never zero cost."""
    if kind == "conformance":
        cases = report.get("cases", [])
        if len(cases) != 21 or len({case["name"] for case in cases}) != 21:
            raise ProofError("conformance must retain all 21 distinct case rows")
        rows = [case.get("evidence", {}) for case in cases if case["status"] != "SKIPPED"]
        if any(row.get("peak_rss_bytes", 0) <= 0 or "cpu_total_ms" not in row for row in rows):
            raise EnvironmentBlocked("conformance client resource evidence is unavailable")
        return
    rows = ([row for group in report["samples"].values() for row in group]
            if kind == "warm" else report["samples"])
    if not rows or any(row.get("client_peak_rss_kib", 0) <= 0 for row in rows):
        raise EnvironmentBlocked(f"{kind} client peak RSS is unavailable")
    if kind in {"one_shot", "warm"} and any(
        row.get("client_sampled_cpu_lower_bound_ms") is None
        or row.get("client_cpu_sample_count", 0) <= 0 for row in rows
    ):
        raise EnvironmentBlocked(f"{kind} native own-client CPU samples are unavailable")


def sample_metrics(kind: str, report: dict[str, Any]) -> dict[str, list[float]]:
    groups = report["samples"] if kind == "warm" else {"": report.get("samples", [])}
    if kind == "conformance":
        groups = {case["name"]: [case["evidence"]]
                  for case in report["cases"] if case.get("evidence")}
    result = {}
    fields = ("wall_ms", "client_cpu_ms", "client_sampled_cpu_lower_bound_ms",
              "process_tree_cpu_ms", "client_peak_rss_kib", "harness_wall_ms",
              "cpu_user_ms", "cpu_sys_ms", "cpu_total_ms", "peak_rss_bytes")
    for shape, rows in groups.items():
        for field in fields:
            values = [float(row[field]) for row in rows if row.get(field) is not None]
            if values:
                prefix = f"{kind}/{shape}" if shape else kind
                result[f"{prefix}/{field}"] = values
    if kind == "one_shot":
        normalized_tree = report.get("summary", {}).get("process_tree_cpu_total_21_normalized_ms")
        if normalized_tree is not None:
            result["one_shot/process_tree_cpu_total_21_normalized_ms"] = [float(normalized_tree)]
        own = result.get("one_shot/client_sampled_cpu_lower_bound_ms", [])
        if own and len(own) == len(report["samples"]):
            result["one_shot/client_sampled_cpu_lower_bound_total_21_normalized_ms"] = [sum(own) * 21 / len(own)]
    return result


def comparisons(samples: dict[str, dict[str, list[float]]]) -> dict[str, Any]:
    result = {}
    for metric in sorted(samples["A"].keys() | samples["B"].keys()):
        before, after = samples["A"].get(metric, []), samples["B"].get(metric, [])
        if not before or not after:
            result[metric] = {"complete": False, "baseline_count": len(before), "candidate_count": len(after)}
            continue
        a, am = median_mad(before)
        b, bm = median_mad(after)
        result[metric] = {"complete": len(before) == len(after), "baseline_count": len(before),
                          "candidate_count": len(after), "baseline_median": a, "baseline_mad": am,
                          "candidate_median": b, "candidate_mad": bm, "delta": b-a,
                          "baseline_max": max(before), "candidate_max": max(after),
                          "baseline_total": sum(before), "candidate_total": sum(after),
                          "neutral_within_mad": b <= a + max(am, bm)}
    return result


def measure(args: argparse.Namespace) -> int:
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    bins = {"A": args.baseline.resolve(), "B": args.candidate.resolve()}
    samples: dict[str, dict[str, list[float]]] = {"A": {}, "B": {}}
    report: dict[str, Any] = {"schema": "haider.thinexe.abba.v1", "order": ORDER, "runs": [],
                              "created_at_utc": datetime.now(timezone.utc).isoformat(),
                              "host": {"platform": platform.platform(), "python": platform.python_version()},
                              "load_limit_one_minute": LOAD_LIMIT, "status": "INCOMPLETE"}
    def save() -> None:
        report["comparison"] = comparisons(samples)
        write_json(output / "abba.json", report)
    try:
        require_quiet_host()
        report["binaries"] = {label: inventory(path, payload_required=label == "B") for label, path in bins.items()}
        boundary = boundary_report(bins["B"])
        write_json(output / "boundary.json", boundary)
        report["boundary_passed"] = boundary["passed"]
        peer = args.conformance_root.resolve()
        if not (peer / "bench/conformance/__main__.py").is_file():
            raise EnvironmentBlocked(f"read-only conformance peer is unavailable: {peer}")
        report["authorities"] = {str(path): sha256_file(path) for path in (
            Path(__file__), Path(__file__).with_name("turnperf_support.py"),
            Path(__file__).with_name("turn_wall_harness.py"),
            peer / "bench/conformance/runner.py", peer / "bench/adapters/haider-agent/adapter.toml",
            peer / "bench/adapters/normalize.py")}
        for index, label in enumerate(ORDER, 1):
            for kind in ("exec", "one_shot", "warm", "conformance"):
                require_quiet_host()
                assert_frozen(report["binaries"])
                print(f"{index}/4 {label} {kind}: starting", flush=True)
                path = output / f"{index}-{label}-{kind}.json"
                if kind == "conformance":
                    raw, proof = run_conformance(peer, bins[label], path)
                    write_json(path.with_name(path.stem + "-proof.json"), proof)
                else:
                    runner = {"exec": exec_floor, "one_shot": run_one_shot_harness, "warm": run_harness}[kind]
                    raw = runner(bins[label])
                    write_json(path, raw)
                    proof = {key: raw[key] for key in ("passed", "measurement_accepted")}
                report["runs"].append({"index": index, "label": label, "kind": kind,
                                       "path": path.name, "sha256": sha256_file(path), **proof})
                save()
                if kind == "conformance" and not proof["strict_load_accepted"]:
                    raise EnvironmentBlocked("conformance crossed the unchanged load<3 pin")
                if kind != "conformance" and not raw["measurement_accepted"]:
                    raise EnvironmentBlocked("; ".join(raw["measurement_reasons"]))
                assert_frozen(report["binaries"])
                require_client_rows(kind, raw)
                for metric, values in sample_metrics(kind, raw).items():
                    samples[label].setdefault(metric, []).extend(values)
                save()
        report["measurements_complete"] = len(report["runs"]) == 16
        for path, expected in report["authorities"].items():
            if sha256_file(Path(path)) != expected:
                raise ProofError(f"measurement authority changed during ABBA: {path}")
        report["measurement_accepted"] = all(row["measurement_accepted"] for row in report["runs"])
        report["status"] = "PASS" if (boundary["passed"] and report["measurement_accepted"]
                                           and all(row["passed"] for row in report["runs"])) else "FAIL"
        report["limitations"] = [
            "Own-client sampled CPU is a lower bound ending at the last live PID sample; process-tree CPU remains separately named.",
            "Turn client RSS is the existing 2 ms live sampler's peak; exec-floor RSS is the wait4 kernel high-water mark.",
            "Conformance CPU/RSS rows are the unchanged peer's wait4 evidence; case FAIL/SKIPPED and measurement rejection are never relabeled.",
            "PASS here means complete accepted measurements/correctness/boundary proof; the owner assesses the approximately 5 MB target and comparative performance separately.",
        ]
    except EnvironmentBlocked as error:
        report.update(status="ENVIRONMENT-BLOCKED", reason=str(error), measurements_complete=False)
    except (ProofError, OSError, ValueError) as error:
        report.update(status="FAIL", reason=str(error), measurements_complete=False)
    save()
    print(f"{report['status']}: {output / 'abba.json'}", flush=True)
    return 0 if report["status"] == "PASS" else 75 if report["status"] == "ENVIRONMENT-BLOCKED" else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    boundary = commands.add_parser("boundary", help="check normal dependencies and the existing candidate artifact; no build")
    boundary.add_argument("--candidate", type=Path, default=ROOT / "target/release")
    boundary.add_argument("--target", default="all")
    boundary.add_argument("--output", type=Path, required=True)
    abba = commands.add_parser("measure", help="run fixed ABBA against two frozen artifact directories")
    abba.add_argument("--baseline", type=Path, default=Path("/private/tmp/thinexe-before"))
    abba.add_argument("--candidate", type=Path, default=ROOT / "target/release")
    abba.add_argument("--conformance-root", type=Path, required=True)
    abba.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "measure":
        return measure(args)
    try:
        report = boundary_report(args.candidate.resolve(), target=args.target)
    except (ProofError, OSError) as error:
        report = {"passed": False, "failures": [str(error)]}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    write_json(args.output, report)
    print(json.dumps({"passed": report["passed"], "failures": report["failures"]}), flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
