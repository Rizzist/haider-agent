#!/usr/bin/env python3
"""Reproduce raw-IPC JSON/binary CAS throughput and exact process peak RSS.

Requires Python blake3 (pip install blake3) and an already-built haiderd.
Each case gets a fresh daemon/profile and a fresh Python IPC client process.
The comparable cases write eight distinct 33 MiB objects (264 MiB total).
The capacity case writes the same concatenated bytes as one 264 MiB object.
The legacy single-object case performs real encoding and sending, records its
frame-limit rejection, and is excluded from successful-work speed comparisons.
Fixtures and their BLAKE3 hashes are prepared before timing; wall measures
file read + client encoding + IPC + durable CAS publication. Peak RSS comes
from getrusage/wait4 and includes each process's startup, not sampled estimates.
This is a raw IPC harness, so client RSS describes Python, not the Rust CLI.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import time

import blake3

MIB = 1024 * 1024
CHUNK = 64 * 1024
FEATURE = "artifact_put_binary_v1"
FRAME_LIMIT = 48 * MIB
STARTUP_DEADLINE_SECONDS = 30


def rss_bytes(usage: resource.struct_rusage) -> int:
    return int(usage.ru_maxrss) * (1 if sys.platform == "darwin" else 1024)


def exact_read(peer: socket.socket, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        part = peer.recv(length - len(result))
        if not part:
            raise EOFError("peer disconnected during frame")
        result.extend(part)
    return bytes(result)


def send_json(peer: socket.socket, value: dict) -> None:
    body = json.dumps(value, separators=(",", ":")).encode()
    peer.sendall(struct.pack("!I", len(body)))
    peer.sendall(body)


def receive(peer: socket.socket) -> dict:
    while True:
        length = struct.unpack("!I", exact_read(peer, 4))[0]
        if length > FRAME_LIMIT:
            raise RuntimeError(f"unexpected inbound frame length {length}")
        frame = json.loads(exact_read(peer, length))
        if frame.get("kind") == "ping":
            send_json(peer, {**frame, "kind": "pong"})
            continue
        if frame.get("kind") == "resident_session_binding":
            continue
        return frame


def connect(endpoint: Path) -> tuple[socket.socket, dict]:
    peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    # Diagnostic bound only: 512 MiB maximum upload / 4 MiB/s minimum
    # benchmark progress allowance = 128 seconds per frame response.
    peer.settimeout(128)
    peer.connect(str(endpoint))
    send_json(peer, {
        "v": 1, "kind": "hello", "protocol_min": 1, "protocol_max": 1,
        "client_name": "casstream-bench", "client_version": "0.0.970",
        "client_instance_id": f"bench-{os.getpid()}", "client_kind": "cli",
        "capabilities_requested": ["view", "control"],
        "max_receive_frame": FRAME_LIMIT,
    })
    welcome = receive(peer)
    if welcome.get("kind") != "welcome":
        raise RuntimeError(f"handshake failed: {welcome}")
    return peer, welcome


def response(peer: socket.socket, request_id: str) -> dict:
    frame = receive(peer)
    if frame.get("kind") != "response" or frame.get("request_id") != request_id:
        raise RuntimeError(f"unexpected upload response: {frame}")
    return frame["body"]


def binary_request(peer: socket.socket, request_id: str, payload: bytes) -> dict:
    identity = request_id.encode("ascii")
    body = bytes([len(identity)]) + identity + payload
    peer.sendall(struct.pack("!I", len(body) | 0x80000000))
    peer.sendall(body)
    return response(peer, request_id)


def send_legacy(peer: socket.socket, request_id: str, raw: bytes) -> None:
    # Keep exactly the source snapshot and its immutable base64 encoding;
    # serialize the small envelope separately instead of copying a huge JSON
    # document repeatedly through Python str/json.dumps/bytes intermediates.
    encoded = base64.b64encode(raw)
    head = (b'{"v":1,"kind":"request","request_id":' + json.dumps(request_id).encode() +
            b',"body":{"method":"artifact.put","data_base64":"')
    tail = b'"}}'
    peer.sendall(struct.pack("!I", len(head) + len(encoded) + len(tail)))
    peer.sendall(head)
    peer.sendall(encoded)
    peer.sendall(tail)


def worker(plan_path: Path) -> None:
    plan = json.loads(plan_path.read_text())
    peer, welcome = connect(Path(plan["endpoint"]))
    mode = plan["mode"]
    if mode.startswith("binary") and FEATURE not in welcome.get("features", []):
        raise RuntimeError("daemon did not advertise binary artifact.put")
    started = time.perf_counter()
    responses = []
    if mode == "legacy_single":
        # This is the actual failed 264 MiB legacy client path: read all input,
        # base64-encode it, frame it, and attempt to send it. A daemon that
        # enforces its 48 MiB frame cap may close before accepting the body.
        raw = Path(plan["source"]).read_bytes()
        send_error = None
        try:
            send_legacy(peer, "legacy-single", raw)
        except (BrokenPipeError, ConnectionResetError) as error:
            send_error = type(error).__name__
        del raw
        try:
            rejection = receive(peer)
        except (EOFError, ConnectionResetError):
            rejection = {"disconnected": True}
        if not rejection.get("disconnected") and not (rejection.get("kind") == "protocol_error" and rejection.get("code") == "invalid_frame" and rejection.get("fatal") is True):
            raise RuntimeError(f"unexpected oversized legacy rejection: {rejection}")
        responses.append({"frame": rejection, "send_error": send_error})
    else:
        objects = plan["parts"] if mode.endswith("eight") else [plan["whole"]]
        with Path(plan["source"]).open("rb") as source:
            for index, obj in enumerate(objects):
                source.seek(obj["offset"])
                if mode == "legacy_eight":
                    raw = source.read(obj["bytes"])
                    if len(raw) != obj["bytes"]:
                        raise RuntimeError("short benchmark fixture")
                    request_id = f"legacy-{index}"
                    send_legacy(peer, request_id, raw)
                    result = response(peer, request_id)
                    del raw
                else:
                    result = binary_request(peer, f"begin-{index}", b"\x01" +
                                            struct.pack("!Q", obj["bytes"]) + obj["digest"].encode())
                    if result != {"method": "artifact.put.progress", "bytes": 0}:
                        raise RuntimeError(f"binary begin rejected: {result}")
                    offset = 0
                    while offset < obj["bytes"]:
                        chunk = source.read(min(CHUNK, obj["bytes"] - offset))
                        if not chunk:
                            raise RuntimeError("short binary fixture")
                        result = binary_request(peer, f"chunk-{index}-{offset}",
                                                b"\x02" + struct.pack("!Q", offset) + chunk)
                        offset += len(chunk)
                        if result != {"method": "artifact.put.progress", "bytes": offset}:
                            raise RuntimeError(f"binary chunk rejected: {result}")
                    result = binary_request(peer, f"finish-{index}", b"\x03")
                expected = {"method": "artifact.put", "artifact": "blake3:" + obj["digest"],
                            "bytes": obj["bytes"]}
                if result != expected:
                    raise RuntimeError(f"put result mismatch: {result} != {expected}")
                responses.append(result)
    elapsed = time.perf_counter() - started
    peer.close()
    print(json.dumps({"wall_seconds": elapsed, "client_peak_rss_bytes": rss_bytes(resource.getrusage(resource.RUSAGE_SELF)),
                      "accepted": mode != "legacy_single", "responses": responses, "welcome_frame_limit": welcome["frame_limit"]}))


def fixture(root: Path, part_bytes: int) -> dict:
    path = root / "source.bin"
    whole_hash = blake3.blake3()
    parts = []
    with path.open("wb") as destination:
        for part in range(8):
            digest = blake3.blake3()
            chunk = bytes([17 + part]) * CHUNK
            remaining = part_bytes
            while remaining:
                current = chunk[:min(CHUNK, remaining)]
                destination.write(current)
                digest.update(current)
                whole_hash.update(current)
                remaining -= len(current)
            parts.append({"offset": part * part_bytes, "bytes": part_bytes, "digest": digest.hexdigest()})
    return {"source": str(path), "parts": parts, "total_bytes": 8 * part_bytes,
            "whole": {"offset": 0, "bytes": 8 * part_bytes, "digest": whole_hash.hexdigest()}}


def reap_daemon(process: subprocess.Popen) -> resource.struct_rusage:
    process.send_signal(signal.SIGTERM)
    # wait4 returns exact lifetime peak RSS for this daemon alone. Unlike
    # RUSAGE_CHILDREN, a later case cannot inherit an earlier process's peak.
    _, status, usage = os.wait4(process.pid, 0)
    process.returncode = os.waitstatus_to_exitcode(status)
    return usage


def verify_objects(profile: Path, plan: dict) -> None:
    if plan["mode"] == "legacy_single":
        digest = plan["whole"]["digest"]
        if (profile / "cas" / digest[:2] / digest).exists():
            raise RuntimeError("rejected legacy attempt unexpectedly published its object")
        return
    objects = plan["parts"] if plan["mode"].endswith("eight") else [plan["whole"]]
    for obj in objects:
        digest = obj["digest"]
        path = profile / "cas" / digest[:2] / digest
        hasher = blake3.blake3()
        total = 0
        with path.open("rb") as source:
            while chunk := source.read(CHUNK):
                hasher.update(chunk)
                total += len(chunk)
        if total != obj["bytes"] or hasher.hexdigest() != digest:
            raise RuntimeError(f"stored digest integrity failed: {digest}")


def run_case(args: argparse.Namespace, source: dict, index: int, mode: str) -> dict:
    load_start = os.getloadavg()[0]
    if load_start >= args.max_load:
        raise RuntimeError(f"ENVIRONMENT-BLOCKED: load {load_start:.2f} >= {args.max_load}")
    with tempfile.TemporaryDirectory(prefix="hcs-", dir="/private/tmp" if sys.platform == "darwin" else "/tmp") as temporary:
        root = Path(temporary)
        profile, runtime, fixture_home = root / "p", root / "r", root / "h"
        profile.mkdir(mode=0o700)
        runtime.mkdir(mode=0o700)
        fixture_home.mkdir(mode=0o700)
        plan = {**source, "mode": mode, "endpoint": str(runtime / "h.sock")}
        plan_path = root / "plan.json"
        plan_path.write_text(json.dumps(plan))
        env = os.environ.copy()
        # Production lockdown is user-global even with explicit profile flags.
        # Give only these child processes a hermetic home; never mutate the
        # invoking process environment or touch the operator's ~/.haider.
        env.update({"HAIDER_DISCOVERY_DISABLED": "1", "HAIDER_TEST_DEVICE_NAME": "test-mac",
                    "HAIDER_NO_UPDATE_CHECK": "1", "RUST_MIN_STACK": "8388608",
                    "HOME": str(fixture_home), "USERPROFILE": str(fixture_home)})
        daemon_log_path = args.output.parent / f"{args.output.stem}-{index}-{mode}.daemon.log"
        with daemon_log_path.open("w") as daemon_log:
            daemon = subprocess.Popen([str(args.daemon), "--profile", "casstream-bench", "--store-dir", str(profile),
                                       "--runtime-dir", str(runtime)], env=env, stdout=daemon_log, stderr=subprocess.STDOUT)
            try:
                # One daemon readiness phase × the existing product's 30 s
                # STARTUP_DEADLINE (haider-client/src/spawn.rs).
                deadline = time.monotonic() + STARTUP_DEADLINE_SECONDS
                while not Path(plan["endpoint"]).exists():
                    if daemon.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f"daemon startup failed; inspect {daemon_log_path}")
                    time.sleep(0.01)
                client = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--worker", str(plan_path)],
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
                loads = [load_start]
                while client.poll() is None:
                    loads.append(os.getloadavg()[0])
                    time.sleep(0.05)
                output, error = client.communicate()
                if client.returncode != 0:
                    raise RuntimeError(f"client failed: {error.strip()}")
                result = json.loads(output)
                verify_objects(profile, plan)
            finally:
                if daemon.returncode is None:
                    usage = reap_daemon(daemon)
                else:
                    raise RuntimeError(f"daemon exited early: {daemon.returncode}; inspect {daemon_log_path}")
        return {"run": index, "mode": mode, "total_bytes": source["total_bytes"],
                "load_1m_start": load_start, "load_1m_max": max(loads), "load_valid": max(loads) < args.max_load,
                "daemon_peak_rss_bytes": rss_bytes(usage), "daemon_exit_code": daemon.returncode,
                "digest_integrity": "verified" if mode != "legacy_single" else "rejected object absent",
                **result}


def summary(document: dict) -> dict:
    metrics = ("wall_seconds", "client_peak_rss_bytes", "daemon_peak_rss_bytes")
    groups = {}
    for mode in sorted({sample["mode"] for sample in document["samples"]}):
        samples = [sample for sample in document["samples"] if sample["mode"] == mode]
        values = {metric: [sample[metric] for sample in samples] for metric in metrics}
        medians = {metric: statistics.median(data) for metric, data in values.items()}
        groups[mode] = {
            "n": len(samples), "accepted": all(sample["accepted"] for sample in samples),
            "median": medians,
            "range": {metric: [min(data), max(data)] for metric, data in values.items()},
            "mad": {metric: statistics.median(abs(value - medians[metric]) for value in data)
                    for metric, data in values.items()},
        }
    result = {"daemon_sha256": document["daemon_sha256"], "daemon_bytes": document["daemon_bytes"],
              "load_1m_max": max(sample["load_1m_max"] for sample in document["samples"]),
              "valid": document.get("valid", False), "groups": groups}
    if "legacy_eight" in groups and "binary_eight" in groups:
        before, after = groups["legacy_eight"]["median"], groups["binary_eight"]["median"]
        result["matched_work_change_percent"] = {
            metric: 100 * (after[metric] / before[metric] - 1) for metric in metrics}
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--summarize", type=Path, help="read existing measurements and write median/range/MAD/delta summary to --output")
    parser.add_argument("--daemon", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--max-load", type=float, default=10)
    parser.add_argument("--smoke", action="store_true", help="one 8 MiB smoke pass; not reportable performance evidence")
    args = parser.parse_args()
    if args.worker:
        worker(args.worker)
        return
    if args.summarize:
        if not args.output or args.output.exists():
            parser.error("summary requires a new --output path, preserving original evidence")
        document = json.loads(args.summarize.read_text())
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(summary(document), indent=2) + "\n")
        return
    if not args.daemon or not args.output or (args.runs < 3 and not args.smoke):
        parser.error("--daemon and --output required; reportable --runs must be >=3")
    if args.max_load <= 0 or args.max_load > 10:
        parser.error("--max-load must be positive and cannot exceed the lane's limit of 10")
    if not args.daemon.is_file() or args.daemon.stat().st_size <= 10 * MIB:
        parser.error("haiderd must be an existing binary exceeding 10 MiB")
    args.daemon = args.daemon.resolve()
    args.output = args.output.resolve()
    if args.output.exists():
        parser.error("output already exists; choose a new name to preserve every raw trial")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    binary_hash = hashlib.sha256()
    with args.daemon.open("rb") as binary:
        while chunk := binary.read(CHUNK):
            binary_hash.update(chunk)
    document = {"schema": "casstream-benchmark-v1", "platform": sys.platform,
                "machine": platform.machine(), "os_release": platform.release(), "cpu_count": os.cpu_count(),
                "measurement": "fresh Python IPC client self getrusage + fresh daemon wait4, peak RSS includes startup; wall excludes fixture/startup",
                "python": sys.version, "blake3_version": blake3.__version__,
                "smoke": args.smoke, "daemon": str(args.daemon), "daemon_sha256": binary_hash.hexdigest(),
                "daemon_bytes": args.daemon.stat().st_size, "samples": []}
    with tempfile.TemporaryDirectory(prefix="casstream-fixture-") as temporary:
        source = fixture(Path(temporary), MIB if args.smoke else 33 * MIB)
        document["fixture"] = {key: value for key, value in source.items() if key != "source"}
        for index in range(1, (1 if args.smoke else args.runs) + 1):
            modes = ["legacy_eight", "binary_eight", "binary_single"]
            if not args.smoke:
                modes.insert(0, "legacy_single")
            if index % 2 == 0:
                modes.reverse()
            for mode in modes:
                result = run_case(args, source, index, mode)
                document["samples"].append(result)
                args.output.write_text(json.dumps(document, indent=2) + "\n")
                print(json.dumps(result), flush=True)
        document["medians"] = {
            mode: {metric: statistics.median(sample[metric] for sample in document["samples"] if sample["mode"] == mode)
                   for metric in ("wall_seconds", "client_peak_rss_bytes", "daemon_peak_rss_bytes")}
            for mode in sorted({sample["mode"] for sample in document["samples"]})}
        document["valid"] = not args.smoke and all(sample["load_valid"] for sample in document["samples"])
        document["summary"] = summary(document)
        args.output.write_text(json.dumps(document, indent=2) + "\n")


if __name__ == "__main__":
    main()
