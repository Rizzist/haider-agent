#!/usr/bin/env python3
"""Real local peer delivery probe; synthetic daemons and an explicit bridge only.

Requires built target/debug/{haider,haiderd}, Python blake3/msgpack, and Unix.
No provider credentials, signing files, or existing daemon profiles are used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time

import blake3
import msgpack
from gate.tui_probe import RpcClient


def wait_for(check, label, timeout=35):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError(f"timed out: {label}")


def receive(wire):
    def exact(size):
        data = b""
        while len(data) < size:
            part = wire.recv(size - len(data))
            if not part:
                raise EOFError()
            data += part
        return data
    size = struct.unpack(">I", exact(4))[0]
    assert size <= 128 * 1024
    return json.loads(exact(size))


def send_frame(wire, frame):
    data = json.dumps(frame, separators=(",", ":")).encode()
    wire.sendall(struct.pack(">I", len(data)) + data)


class Bridge:
    def __init__(self, runtime, root, identity="fixture-bridge", features=True):
        self.identity, self.device = identity, "fixture-device"
        self.path = runtime / ("px-" + blake3.blake3(identity.encode()).hexdigest()[:12] + ".s")
        self.db = root / (identity + ".sqlite")
        self.mode, self.features = "drop_reply", features
        self.log, self.lock, self.stop_event = [], threading.Lock(), threading.Event()
        self.listener, self.thread = None, None
        with sqlite3.connect(self.db) as db:
            db.execute("CREATE TABLE IF NOT EXISTS inbox(sender TEXT, msg_id TEXT, message TEXT, PRIMARY KEY(sender,msg_id))")
        manifest = dict(version=1, id=identity, device_id=self.device, name=identity,
                        kind="external", socket=self.path.name,
                        capabilities=["peer_agent_injection_v1"] if features else [],
                        state="idle", started_at=1, last_seen=1)
        self.path.with_suffix(".j").write_text(json.dumps(manifest))
        self.path.with_suffix(".j").chmod(0o600)

    def start(self):
        self.path.unlink(missing_ok=True)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.path))
        self.path.chmod(0o600)
        self.listener.listen(16)
        self.listener.settimeout(0.2)
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop_event.is_set():
            try:
                wire, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with wire:
                wire.settimeout(3)
                try:
                    hello = receive(wire)
                    assert hello["kind"] == "hello"
                    send_frame(wire, dict(v=1, kind="welcome", protocol=1, instance_id=self.identity,
                        daemon_generation=1, frame_limit=131072, profile_id=self.device,
                        daemon_version="synthetic-bridge", lifecycle_phase="ready",
                        capabilities_granted=[], features=["peer_agent_injection_v1", "peer_messaging_v1"] if self.features else [],
                        user_command_withheld=False))
                    frame = receive(wire)
                    while frame["kind"] == "ping":
                        send_frame(wire, dict(v=1, kind="pong", nonce=frame["nonce"]))
                        frame = receive(wire)
                    assert frame["body"]["method"] == "peer.inject"
                    message = frame["body"]["message"]
                    sender = message["from"]["id"] + "@" + message["from"]["device_id"]
                    with self.lock:
                        self.log.append(dict(msg_id=message["msg_id"], sender=sender, mode=self.mode, message=message))
                        if self.mode != "approval":
                            with sqlite3.connect(self.db) as db:
                                db.execute("INSERT OR IGNORE INTO inbox VALUES (?,?,?)", (sender, message["msg_id"], message["message"]))
                    if self.mode == "drop_reply":
                        continue
                    state = "held_for_approval" if self.mode == "approval" else "delivered"
                    receipt = dict(msg_id=message["msg_id"], delivery="queued" if state == "held_for_approval" else "delivered",
                        status=dict(state=state, reason="fixture owner approval pending" if state == "held_for_approval" else None,
                            to=self.identity, accepted_at_ms=message["queued_at"], updated_at_ms=int(time.time()*1000)))
                    send_frame(wire, dict(v=1, kind="response", request_id=frame["request_id"], body=dict(method="peer.send", receipt=receipt)))
                except (EOFError, OSError):
                    pass

    def count(self, msg_id):
        with sqlite3.connect(self.db) as db:
            return db.execute("SELECT count(*) FROM inbox WHERE msg_id=?", (msg_id,)).fetchone()[0]

    def close(self):
        self.stop_event.set()
        if self.listener:
            self.listener.close()
        if self.thread:
            self.thread.join(5)
        self.path.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    repo, evidence = args.repo.resolve(), args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    binaries = [repo / "target/debug/haider", repo / "target/debug/haiderd"]
    artifact_hashes = {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in binaries}
    commands, checks, processes, bridges, clients = [], [], [], [], []
    root = Path(tempfile.mkdtemp(prefix="hpd-", dir="/tmp"))
    runtime = root / "rv"
    runtime.mkdir(mode=0o700)
    envs = {}
    try:
        def launch(name):
            profile, runtime_root = root / name, root / (name + "-runtime")
            probe_home = root / (name + "-home")
            profile.mkdir(exist_ok=True)
            runtime_root.mkdir(exist_ok=True)
            probe_home.mkdir(mode=0o700, exist_ok=True)
            env = {"PATH": os.environ["PATH"], "HOME": str(probe_home),
                "HAIDER_PROFILE_DIR": str(profile), "HAIDER_RUNTIME_DIR": str(runtime_root),
                "HAIDER_PEER_RENDEZVOUS_DIR": str(runtime), "HAIDER_DISCOVERY_DISABLED": "1", "HAIDER_NO_UPDATE_CHECK": "1",
                "HAIDER_TEST_FAKE_PROVIDER": '[{"step":"emit_text","text":"synthetic acknowledgement"},{"step":"finish","reason":"end_turn"}]',
                "RUST_MIN_STACK": "8388608", "NO_COLOR": "1"}
            envs[name] = env
            log = open(evidence / f"{name}-daemon-{len(processes)}.log", "w")
            process = subprocess.Popen([str(binaries[1])], env=env, cwd=root, stdout=log, stderr=subprocess.STDOUT)
            processes.append((process, log))
            def ready():
                if process.poll() is not None:
                    raise AssertionError(f"{name} daemon exited {process.returncode}")
                for path in runtime_root.rglob("h.sock"):
                    try:
                        client = RpcClient(str(path))
                        clients.append(client)
                        return client
                    except (OSError, RuntimeError):
                        pass
            return process, wait_for(ready, name + " ready")

        def stop(process):
            if process.poll() is None:
                process.terminate()
                process.wait(30)

        def cli(name, *args, expected=0):
            result = subprocess.run([str(binaries[0]), "peer", *args], env=envs[name], cwd=root, capture_output=True, text=True, timeout=75)
            commands.append(dict(command=[str(binaries[0]), "peer", *args], profile=name, exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr))
            assert result.returncode == expected, commands[-1]
            return [json.loads(line) for line in result.stdout.splitlines() if line.strip()]

        def status(sender, msg_id):
            pages = cli("a", "status", "--session", sender, msg_id)
            return [receipt for page in pages for receipt in page["status"]["receipts"]]

        a, arpc = launch("a")
        b, brpc = launch("b")
        sender, _ = arpc.create_session(root)
        receiver, _ = brpc.create_session(root)
        arpc.close()
        brpc.close()
        peers = wait_for(lambda: cli("a", "list", "--json")[0]["agents"], "peer roster")
        address = wait_for(lambda: next((p for p in cli("a", "list", "--json")[0]["agents"] if p["id"] == receiver), None), "receiver registration")
        address = f"session:{receiver}@{address['device_id']}"
        stop(b)
        held = cli("a", "send", "--session", sender, "--id", "real-offline", address, "synthetic offline message")[0]
        assert held["status"]["state"] == "held", held
        checks.append("two separate profiles/runtimes; authorized rendezvous; offline holding")
        stop(a)
        a, arpc = launch("a")
        b, brpc = launch("b")
        arpc.attach_control(sender)
        arpc.close()
        brpc.attach_control(receiver)
        delivered = wait_for(lambda: next((r for r in status(sender, "real-offline") if r["status"]["state"] == "delivered"), None), "offline delivery after both restart")
        assert delivered["status"]["accepted_at_ms"] == held["status"]["accepted_at_ms"]
        replay = cli("a", "send", "--session", sender, "--id", "real-offline", address, "synthetic offline message")[0]
        assert replay == delivered
        checks.append("both daemons restarted; stable address/msg_id; receipt replay")
        brpc.close()
        # name is a text command, so capture it separately.
        result = subprocess.run([str(binaries[0]), "peer", "name", "--session", receiver, "renamed-receiver"], env=envs["b"], cwd=root, capture_output=True, text=True, timeout=75)
        commands.append(dict(command=["haider", "peer", "name", "--session", receiver, "renamed-receiver"], exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr))
        assert result.returncode == 0
        again = cli("a", "send", "--session", sender, "--id", "after-rename", address, "synthetic stable address after rename")[0]
        assert again["status"]["state"] == "delivered"
        checks.append("rename preserves address")

        bridge = Bridge(runtime, root)
        bridges.append(bridge)
        bridge_address = f"session:{bridge.identity}@{bridge.device}"
        offline = cli("a", "send", "--session", sender, "--id", "lost-reply", bridge_address, "synthetic external bridge message")[0]
        assert offline["status"]["state"] == "held"
        bridge.start()
        wait_for(lambda: bridge.count("lost-reply") == 1, "bridge durable inbox")
        assert status(sender, "lost-reply")[-1]["status"]["state"] == "held"
        stop(a)
        a, arpc = launch("a")
        arpc.attach_control(sender)
        arpc.close()
        bridge.mode = "delivered"
        wait_for(lambda: status(sender, "lost-reply")[-1]["status"]["state"] == "delivered", "lost reply replay")
        assert bridge.count("lost-reply") == 1
        checks.append("external bridge offline queue; lost receiver reply; sender restart; once-only inbox admission")
        bridge.mode = "drop_reply"
        disconnect_rpc = RpcClient(str(next((root / "a-runtime").rglob("h.sock"))))
        clients.append(disconnect_rpc)
        disconnect_rpc.attach_control(sender)
        body = dict(method="peer.send", to=bridge_address, message="synthetic sender disconnect", options=dict(msg_id="disconnect"))
        disconnect_rpc._send(dict(v=1, kind="request", request_id="disconnect-request", body=body))
        wait_for(lambda: any(r["status"]["state"] == "accepted" for r in status(sender, "disconnect")), "sender acceptance before disconnect")
        disconnect_rpc.close()
        commands.append(dict(transport="raw haider-rpc", body=body, action="close sender connection after journaled acceptance"))
        bridge.mode = "delivered"
        wait_for(lambda: status(sender, "disconnect")[-1]["status"]["state"] == "delivered", "delivery survives sender connection close")
        assert bridge.count("disconnect") == 1
        checks.append("sender connection closed after acceptance; queued delivery survives")
        bridge.mode = "approval"
        approval = cli("a", "send", "--session", sender, "--id", "approval", bridge_address, "synthetic held-for-approval message")[0]
        assert approval["status"]["state"] == "held_for_approval"
        assert bridge.count("approval") == 0
        cancelled = cli("a", "status", "--session", sender, "--cancel", "approval", expected=77)[0]
        assert cancelled["status"]["state"] == "failed"
        checks.append("held-for-approval surfaced; cancellation never approves or admits")
        incompatible = Bridge(runtime, root, "incompatible", features=False)
        bridges.append(incompatible)
        incompatible.start()
        failed = cli("a", "send", "--session", sender, "--id", "incompatible", "session:incompatible@fixture-device", "synthetic incompatible message", expected=77)[0]
        assert failed["status"]["state"] == "failed" and "lacks" in failed["status"]["reason"]
        checks.append("incompatible bridge gives journaled failed reason")
        watch_log = open(evidence / "watch.jsonl", "w")
        watch = subprocess.Popen([str(binaries[0]), "peer", "watch", "--session", sender, "lost-reply"], env=envs["a"], cwd=root, stdout=watch_log, stderr=subprocess.PIPE, text=True)
        try:
            wait_for(lambda: (evidence / "watch.jsonl").stat().st_size > 0, "real CLI watch replay")
        finally:
            watch.terminate()
            watch.wait(10)
            watch_log.close()
        checks.append("real CLI list/send/status/watch exercised")
        for name in ("a", "b"):
            journal = []
            for dbpath in (root / name).rglob("*.sqlite*"):
                if dbpath.suffix not in (".sqlite", ".sqlite3"):
                    continue
                with sqlite3.connect(f"file:{dbpath}?mode=ro", uri=True) as db:
                    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='events'").fetchone():
                        continue
                    for session_id, seq, payload_kind, encoded in db.execute("SELECT session_id,seq,payload_kind,envelope_json FROM events WHERE payload_kind IN ('peer.outbox','peer.delivery','peer.message','node_committed') ORDER BY session_id,seq"):
                        if isinstance(encoded, str):
                            envelope = json.loads(encoded)
                        else:
                            try:
                                envelope = msgpack.unpackb(encoded, raw=False)
                            except Exception:
                                envelope = {"encoded_sha256": hashlib.sha256(encoded).hexdigest(), "bytes": len(encoded)}
                        journal.append(dict(session_id=session_id, seq=seq, payload_kind=payload_kind, envelope=envelope))
            (evidence / f"{name}-journal.json").write_text(json.dumps(journal, indent=2))
            assert journal, f"{name}: journal evidence missing"
            if name == "b":
                offline_nodes = [row for row in journal if row["payload_kind"] == "node_committed" and row["envelope"].get("payload", {}).get("kind", {}).get("kind") == "agent" and row["envelope"]["payload"]["kind"].get("message", {}).get("msg_id") == "real-offline"]
                assert len(offline_nodes) == 1, ("receiver admission count", offline_nodes, journal)
                checks.append("receiver journal contains exactly one agent admission for real-offline")

        (evidence / "bridge-requests.json").write_text(json.dumps(bridge.log, indent=2))
        (evidence / "probe-result.json").write_text(json.dumps(dict(verdict="PASS", checks=checks, artifacts=artifact_hashes, bridge_admissions={"lost-reply": bridge.count("lost-reply"), "approval": bridge.count("approval")}, limitation="Synthetic bridge only; no Claude integration or model action completion claimed."), indent=2))
        print(json.dumps(dict(verdict="PASS", checks=checks)))
    finally:
        (evidence / "commands.json").write_text(json.dumps(commands, indent=2))
        for client in clients:
            client.close()
        for bridge in bridges:
            bridge.close()
        for process, log in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(30)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(10)
            log.close()
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
