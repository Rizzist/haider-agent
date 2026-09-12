#!/usr/bin/env python3
"""Real CLI regressions for recipient FIFO and pending-send autospawn liveness.

Derived from the round-1 independent verifier's failing fault-proxy scenarios.
All inputs/profiles are synthetic. Requires msgpack and fresh target/debug
haider + haiderd; writes only new evidence and cleans up owned processes.
"""

import hashlib
import json
import os
import pathlib
import shutil
import signal
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import msgpack
import argparse

W = pathlib.Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--evidence", required=True, type=pathlib.Path)
E = parser.parse_args().evidence.resolve()
E.mkdir(parents=True, exist_ok=False)
sys.path.insert(0, str(W / "scripts/qa-gate"))
from gate.tui_probe import RpcClient


def readframe(s):
    def exact(n):
        b = b""
        while len(b) < n:
            x = s.recv(n - len(b))
            if not x:
                raise EOFError()
            b += x
        return b

    n = struct.unpack(">I", exact(4))[0]
    assert n < 16777216
    return json.loads(exact(n))


def writeframe(s, v):
    b = json.dumps(v, separators=(",", ":")).encode()
    s.sendall(struct.pack(">I", len(b)) + b)


def wait(f, label, timeout=45):
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        last = f()
        if last:
            return last
        time.sleep(0.15)
    raise AssertionError((label, last))


def process_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


class FaultProxy:
    """Moves only this fixture's receiver socket; forwards real Welcome/admission."""

    def __init__(self, path):
        self.path = path
        self.backend = path.with_suffix(".actual")
        path.rename(self.backend)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(path))
        path.chmod(0o600)
        self.listener.listen(32)
        self.listener.settimeout(0.2)
        self.stopped = threading.Event()
        self.drop_requests = set()
        self.log = []
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stopped.is_set():
            try:
                front, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with front:
                front.settimeout(6)
                try:
                    hello = readframe(front)
                    with socket.socket(socket.AF_UNIX) as back:
                        back.settimeout(6)
                        back.connect(str(self.backend))
                        writeframe(back, hello)
                        writeframe(front, readframe(back))
                        request = readframe(front)
                        while request["kind"] == "ping":
                            writeframe(
                                front, dict(v=1, kind="pong", nonce=request["nonce"])
                            )
                            request = readframe(front)
                        mid = request.get("body", {}).get("message", {}).get("msg_id")
                        row = dict(
                            time_ms=int(time.time() * 1000),
                            msg_id=mid,
                            request=request,
                            action="forward",
                        )
                        self.log.append(row)
                        if mid in self.drop_requests:
                            row["action"] = "disconnect_before_receiver_admission"
                            continue
                        writeframe(back, request)
                        response = readframe(back)
                        while response["kind"] == "ping":
                            writeframe(
                                back, dict(v=1, kind="pong", nonce=response["nonce"])
                            )
                            response = readframe(back)
                        row["response"] = response
                        writeframe(front, response)
                except (EOFError, OSError):
                    pass
                except Exception as ex:
                    self.log.append(dict(error=repr(ex)))

    def close(self):
        self.stopped.set()
        self.listener.close()
        self.thread.join(8)
        self.path.unlink(missing_ok=True)
        if self.backend.exists():
            self.backend.rename(self.path)


class Harness:
    def __init__(self):
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="hpv-", dir="/tmp"))
        self.rv = self.root / "rv"
        self.rv.mkdir(mode=0o700)
        self.env = {}
        self.procs = []
        self.active = {}
        self.clients = []
        self.commands = []
        self.checks = []
        self.proxies = []
        self.proxy = None
        self.lifecycle = []
        self.autospawn = []

    def record(self, name, passed, detail):
        self.checks.append(dict(name=name, passed=bool(passed), detail=detail))
        print(json.dumps(self.checks[-1]), flush=True)
        self.save()

    def save(self):
        (E / "commands.json").write_text(json.dumps(self.commands, indent=2) + "\n")
        (E / "checks.json").write_text(json.dumps(self.checks, indent=2) + "\n")
        (E / "lifecycle.json").write_text(json.dumps(self.lifecycle, indent=2) + "\n")

    def launch(self, name):
        for d in (name, name + "-home", name + "-runtime"):
            (self.root / d).mkdir(mode=0o700, exist_ok=True)
        env = dict(
            PATH=os.environ["PATH"],
            HOME=str(self.root / (name + "-home")),
            HAIDER_PROFILE_DIR=str(self.root / name),
            HAIDER_RUNTIME_DIR=str(self.root / (name + "-runtime")),
            HAIDER_PEER_RENDEZVOUS_DIR=str(self.rv),
            HAIDER_DISCOVERY_DISABLED="1",
            HAIDER_NO_UPDATE_CHECK="1",
            HAIDER_TEST_FAKE_PROVIDER='[{"step":"emit_text","text":"synthetic ack"},{"step":"finish","reason":"end_turn"}]',
            RUST_MIN_STACK="8388608",
            NO_COLOR="1",
        )
        self.env[name] = env
        log = (E / f"{name}-daemon-{len(self.procs)}.log").open("w")
        p = subprocess.Popen(
            [str(W / "target/debug/haiderd")],
            env=env,
            cwd=self.root,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        self.procs.append((p, log))
        self.active[name] = p
        self.lifecycle.append(
            dict(
                action="start", profile=name, pid=p.pid, time_ms=int(time.time() * 1000)
            )
        )

        def ready():
            assert p.poll() is None, ("daemon exited", name, p.returncode)
            for path in (self.root / (name + "-runtime")).rglob("h.sock"):
                try:
                    c = RpcClient(str(path))
                    self.clients.append(c)
                    return c
                except (OSError, RuntimeError):
                    pass

        return wait(ready, name + " ready")

    def stop(self, name, hard=False):
        p = self.active[name]
        if p.poll() is None:
            p.kill() if hard else p.terminate()
            p.wait(30)
        self.lifecycle.append(
            dict(
                action="SIGKILL" if hard else "SIGTERM",
                profile=name,
                pid=p.pid,
                exit=p.returncode,
                time_ms=int(time.time() * 1000),
            )
        )
        self.save()

    def cli(self, name, *args, expected=0):
        argv = [str(W / "target/debug/haider"), "peer", *args]
        r = subprocess.run(
            argv,
            env=self.env[name],
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=75,
        )
        self.commands.append(
            dict(
                time_ms=int(time.time() * 1000),
                argv=argv,
                profile=name,
                exit=r.returncode,
                stdout=r.stdout,
                stderr=r.stderr,
            )
        )
        self.save()
        assert expected is None or r.returncode == expected, self.commands[-1]
        return r

    def send(self, mid, text=None, ttl=None, expected=0):
        opts = ["--ttl-ms", str(ttl)] if ttl is not None else []
        r = self.cli(
            "a",
            "send",
            "--session",
            self.sender,
            "--id",
            mid,
            *opts,
            self.address,
            text or ("synthetic " + mid),
            expected=expected,
        )
        return json.loads(r.stdout)

    def status(self, mid):
        r = self.cli("a", "status", "--session", self.sender, mid)
        return [
            x
            for l in r.stdout.splitlines()
            for x in json.loads(l)["status"]["receipts"]
        ]

    def delivered(self, mid):
        return wait(
            lambda: next(
                (r for r in self.status(mid) if r["status"]["state"] == "delivered"),
                None,
            ),
            mid + " delivered",
        )

    def journal(self, name):
        rows = []
        for path in (self.root / name).rglob("*.sqlite*"):
            if path.suffix not in (".sqlite", ".sqlite3"):
                continue
            with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
                if not db.execute(
                    "SELECT 1 FROM sqlite_master WHERE name='events'"
                ).fetchone():
                    continue
                for sid, seq, kind, b in db.execute(
                    "SELECT session_id,seq,payload_kind,envelope_json FROM events ORDER BY session_id,seq"
                ):
                    v = (
                        json.loads(b)
                        if isinstance(b, str)
                        else msgpack.unpackb(b, raw=False)
                    )
                    rows.append(
                        dict(session_id=sid, seq=seq, payload_kind=kind, envelope=v)
                    )
        (E / f"{name}-journal.json").write_text(json.dumps(rows, indent=2) + "\n")
        return rows

    def nodes(self):
        return [
            r["envelope"]["payload"]["kind"]["message"]
            for r in self.journal("b")
            if r["payload_kind"] == "node_committed"
            and r["envelope"]["payload"].get("kind", {}).get("kind") == "agent"
        ]

    def receiver_path(self):
        def ready():
            path = next(
                (
                    p.with_suffix(".s")
                    for p in self.rv.glob("ph-*.j")
                    if json.loads(p.read_text())["id"] == self.receiver
                ),
                None,
            )
            if path is None:
                return None
            try:
                with socket.socket(socket.AF_UNIX) as probe:
                    probe.settimeout(0.5)
                    probe.connect(str(path))
                return path
            except OSError:
                return None

        return wait(ready, "receiver session endpoint accepting connections")

    def start_proxy(self):
        self.proxy = FaultProxy(self.receiver_path())
        self.proxies.append(self.proxy)
        return self.proxy

    def stop_proxy(self):
        if self.proxy:
            self.proxy.close()
            self.proxy = None

    def close(self):
        self.stop_proxy()
        for pid, path in self.autospawn:
            if path.exists():
                os.kill(pid, signal.SIGTERM)
                wait(lambda: not path.exists(), "owned autospawn socket cleanup")
            wait(lambda: not process_alive(pid), "owned autospawn process exit")
        for logpath in self.root.rglob("*.log"):
            (
                E
                / ("autospawn-" + str(logpath.relative_to(self.root)).replace("/", "_"))
            ).write_bytes(logpath.read_bytes())
        for c in self.clients:
            c.close()
        for p, log in reversed(self.procs):
            if p.poll() is None:
                p.terminate()
                try:
                    p.wait(30)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait(10)
            log.close()
        for n in ("a", "b"):
            self.journal(n)
        (E / "proxy-transcript.json").write_text(
            json.dumps([r for p in self.proxies for r in p.log], indent=2) + "\n"
        )
        self.save()
        shutil.rmtree(self.root)
        (E / "cleanup.json").write_text(
            json.dumps(
                dict(
                    removed_root=str(self.root),
                    processes=[
                        dict(pid=p.pid, exit=p.returncode) for p, l in self.procs
                    ],
                    autospawn=[
                        dict(pid=pid, alive=process_alive(pid))
                        for pid, path in self.autospawn
                    ],
                ),
                indent=2,
            )
            + "\n"
        )


def main():
    h = Harness()
    artifacts = {
        str(p): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in (W / "target/debug/haider", W / "target/debug/haiderd")
    }
    (E / "artifacts.json").write_text(json.dumps(artifacts, indent=2) + "\n")
    try:
        a = h.launch("a")
        b = h.launch("b")
        h.sender, _ = a.create_session(h.root)
        h.receiver, _ = b.create_session(h.root)
        a.close()
        b.close()
        target = wait(
            lambda: next(
                (
                    p
                    for p in json.loads(h.cli("a", "list", "--json").stdout)["agents"]
                    if p["id"] == h.receiver
                ),
                None,
            ),
            "receiver roster",
        )
        h.address = f"session:{h.receiver}@{target['device_id']}"
        proxy = h.start_proxy()
        proxy.drop_requests.add("order-first")
        first = h.send("order-first")
        second = h.send("order-second")
        # Force a background retry while the head still fails. It must neither
        # forward nor admit the tail, even though that tail's transport works.
        wait(
            lambda: sum(row.get("msg_id") == "order-first" for row in proxy.log) >= 2,
            "head retried while proxy disconnected",
        )
        before = [m["msg_id"] for m in h.nodes()]
        forwarded = [row.get("msg_id") for row in proxy.log]
        h.record(
            "new send and retry both wait behind disconnected FIFO head",
            first["status"]["state"] == second["status"]["state"] == "held"
            and "order-second" not in before
            and "order-second" not in forwarded,
            dict(
                first=first,
                second=second,
                admitted_before_release=before,
                attempted=forwarded,
            ),
        )
        # Preserve the held queue across a real SIGKILL/restart too.
        h.stop("a", hard=True)
        a = h.launch("a")
        a.close()
        proxy.drop_requests.clear()
        h.delivered("order-first")
        h.delivered("order-second")
        actual = [
            m["msg_id"]
            for m in h.nodes()
            if m["msg_id"] in ("order-first", "order-second")
        ]
        h.record(
            "forwarding proxy reconnect preserves recipient FIFO after restart",
            actual == ["order-first", "order-second"],
            actual,
        )
        h.stop_proxy()

        one = h.send("per-sender-id", text="synthetic sender one visible message")
        a = RpcClient(str(next((h.root / "a-runtime").rglob("h.sock"))))
        h.clients.append(a)
        a.counter = 100
        second_sender, _ = a.create_session(h.root)
        a.close()
        primary_sender = h.sender
        h.sender = second_sender
        two = h.send("per-sender-id", text="synthetic sender two visible message")
        h.sender = primary_sender
        h.record(
            "two real senders with the same ID are both admitted for TUI replay",
            one["status"]["state"] == two["status"]["state"] == "delivered"
            and sum(m["msg_id"] == "per-sender-id" for m in h.nodes()) == 2,
            dict(first=one, second=two, second_sender=second_sender),
        )

        h.stop("b")
        h.stop("a")
        h.env["a"]["HAIDER_DAEMON_TRACE"] = "1"
        automatic = h.send("automatic-pending")
        auto_path = next((h.root / "a-runtime").rglob("h.sock"))
        client = RpcClient(str(auto_path))
        h.clients.append(client)
        snapshot = client.request(dict(method="status.snapshot"))
        client.close()
        pid = snapshot["daemon_pid"]
        assert isinstance(pid, int)
        h.autospawn.append((pid, auto_path))
        h.lifecycle.append(
            dict(action="CLI autospawn", profile="a", pid=pid, snapshot=snapshot)
        )
        # No sender CLI polling/attachments during the actual default 30s TTL.
        start = time.monotonic()
        while auto_path.exists() and time.monotonic() - start < 35:
            time.sleep(0.2)
        alive = auto_path.exists()
        h.record(
            "default 30-second CLI idle deadline retains pending outbox",
            alive and automatic["status"]["state"] == "held",
            dict(
                initial=automatic, pid=pid, alive_after_seconds=time.monotonic() - start
            ),
        )
        b = h.launch("b")
        b.attach_control(h.receiver)
        b.close()
        wait(
            lambda: sum(m["msg_id"] == "automatic-pending" for m in h.nodes()) == 1,
            "autospawn delivery after receiver reconnect",
        )
        h.record(
            "receiver reconnect delivers without waking sender via CLI",
            alive,
            dict(receiver_admissions=1, sender_pid=pid),
        )
        wait(lambda: not auto_path.exists(), "terminal delivery wakes idle retirement")
        wait(lambda: not process_alive(pid), "autospawn daemon process retired")
        receipts = [
            r["envelope"]["payload"]
            for r in h.journal("a")
            if r["payload_kind"] == "peer.delivery"
            and r["envelope"]["payload"]["msg_id"] == "automatic-pending"
        ]
        h.record(
            "terminal receipt permits automatic daemon retirement",
            receipts[-1]["status"]["state"] == "delivered",
            receipts,
        )
    except Exception:
        (E / "exception.txt").write_text(traceback.format_exc())
        raise
    finally:
        h.close()
    failures = [check for check in h.checks if not check["passed"]]
    (E / "verdict.json").write_text(
        json.dumps(
            dict(
                verdict="FAIL" if failures else "PASS",
                checks=len(h.checks),
                failures=failures,
            ),
            indent=2,
        )
        + "\n"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
