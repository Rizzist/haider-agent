"""Real disposable-profile UDS close audit; JSON framing, no account discovery.

Counts true open descriptors separately from macOS's allocated pbi_nfiles slots.
Wait windows: explicit close joins session work; 15 seconds = existing 5-second
derived-cache delay + Tokio's 10-second blocking-worker idle keepalive. This is
a resource lifecycle test, not a latency/CPU benchmark or AHRB result rewrite.
"""

import argparse
import hashlib
import json
import os
import pathlib
import shutil
import socket
import struct
import subprocess
import tempfile
import time
from collections import deque


class Client:

    def __init__(self, endpoint, trace):
        self.trace = trace
        self.trace_count = 0
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(60)
        self.socket.connect(str(endpoint))
        self.seq = 0
        self.events = deque()
        self.send(
            dict(
                kind="hello",
                protocol_min=1,
                protocol_max=1,
                client_name="session-close-audit",
                client_version="fixture",
                client_instance_id="audit",
                client_kind="cli",
                capabilities_requested=["view", "control"],
                max_receive_frame=16 * 1024 * 1024,
            )
        )
        self.welcome = self.recv()
        assert self.welcome["kind"] == "welcome", self.welcome

    def send(self, message):
        self.record("send", message)
        data = json.dumps(dict(v=1, **message)).encode()
        self.socket.sendall(struct.pack(">I", len(data)) + data)

    def read(self, length):
        data = b""
        while len(data) < length:
            part = self.socket.recv(length - len(data))
            if not part:
                raise RuntimeError("daemon disconnected")
            data += part
        return data

    def recv(self):
        message = json.loads(self.read(struct.unpack(">I", self.read(4))[0]))
        self.record("receive", message)
        return message

    def record(self, direction, message):
        if self.trace_count < 300:
            with self.trace.open("a") as out:
                out.write(json.dumps(dict(direction=direction, message=message)) + "\n")
            self.trace_count += 1

    def event(self):
        while True:
            frame = self.events.popleft() if self.events else self.recv()
            if frame.get("kind") == "event":
                return frame
            if frame.get("kind") == "ping":
                self.send(dict(kind="pong", nonce=frame["nonce"]))

    def call(self, method, **fields):
        self.seq += 1
        request = str(self.seq)
        self.send(
            dict(kind="request", request_id=request, body=dict(method=method, **fields))
        )
        while True:
            reply = self.recv()
            if reply.get("kind") == "event":
                self.events.append(reply)
            if reply.get("kind") == "ping":
                self.send(dict(kind="pong", nonce=reply["nonce"]))
            if reply.get("kind") == "response" and reply["request_id"] == request:
                if reply["body"]["method"] == "error":
                    raise RuntimeError(reply)
                return reply["body"]


def resources(sampler, pid, label):
    result = subprocess.run(
        [str(sampler.resolve()), str(pid)], capture_output=True, text=True
    )
    if result.returncode:
        raise RuntimeError(("resources", result.returncode, result.stderr))
    fds, threads, slots = ([], [], None)
    for line in result.stdout.splitlines():
        row = line.split("\t")
        if row[0] == "fd":
            fds.append(dict(fd=int(row[1]), type=int(row[2]), path=row[3]))
        if row[0] == "thread":
            threads.append(dict(id=row[1], name=row[2]))
        if row[0] == "allocated_fd_slots":
            slots = int(row[1])
    return dict(
        label=label,
        monotonic=time.monotonic(),
        fd_count=len(fds),
        thread_count=len(threads),
        allocated_fd_slots=slots,
        fds=fds,
        threads=threads,
        load=os.getloadavg(),
    )


def envelope_digest(envelope):
    return hashlib.sha256(json.dumps(envelope, sort_keys=True).encode()).hexdigest()


def verify_preserved_histories(client, histories):
    """Re-read every durable envelope after all subsequent turns and closes."""
    verified = 0
    for session_id, expected in histories.items():
        actual = {}
        for start in range(1, max(expected) + 1, 256):
            page = client.call(
                "session.read",
                session_id=session_id,
                range=dict(start_seq=start, end_seq=start + 255),
            )
            for envelope in page["result"]["envelopes"]:
                actual[envelope["seq"]] = envelope_digest(envelope)
        assert actual == expected, ("final durable history differs", session_id)
        verified += len(actual)
    return verified


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=pathlib.Path)
    parser.add_argument("--output-dir", type=pathlib.Path, required=True)
    parser.add_argument("--sampler", type=pathlib.Path, required=True)
    parser.add_argument("--retain-profile", action="store_true")
    parser.add_argument("--native-close", action="store_true")
    parser.add_argument("--sessions", type=int, default=1000)
    parser.add_argument(
        "--reuse-session",
        action="store_true",
        help="reopen one durable session across all turns",
    )
    args = parser.parse_args()
    if args.sessions < 1:
        parser.error("--sessions must be positive")
    out = args.output_dir
    out.mkdir(exist_ok=False)
    root = pathlib.Path(tempfile.mkdtemp(prefix="hclose-", dir="/tmp"))
    env = dict(
        PATH=os.environ["PATH"],
        HOME=str(root / "home"),
        TMPDIR=str(root),
        RUST_MIN_STACK="8388608",
        HAIDER_DISCOVERY_DISABLED="1",
        HAIDER_TEST_DEVICE_NAME="test-mac",
        HAIDER_TEST_FAKE_PROVIDER=json.dumps(
            [
                dict(step="emit_text", text="retention-needle"),
                dict(step="finish", reason="end_turn"),
            ]
            * (args.sessions + 4)
        ),
    )
    (root / "home").mkdir()
    command = [
        str(args.binary.resolve()),
        "--profile",
        "session-close-audit",
        "--store-dir",
        str(root / "store"),
        "--runtime-dir",
        str(root / "runtime"),
    ]
    record = dict(
        command=command,
        binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        root=str(root),
        native_close=args.native_close,
        sessions=args.sessions,
        reuse_session=args.reuse_session,
        snapshots=[],
    )
    client = None
    with (out / "daemon.log").open("w") as log:
        daemon = subprocess.Popen(
            command, env=env, stdout=log, stderr=subprocess.STDOUT
        )
        record["pid"] = daemon.pid
        try:
            until = time.monotonic() + 60
            while not list((root / "runtime").rglob("*.sock")):
                if daemon.poll() is not None:
                    raise RuntimeError(("daemon start", daemon.returncode))
                if time.monotonic() > until:
                    raise TimeoutError("daemon readiness watchdog")
                time.sleep(0.05)
            client = Client(
                next((root / "runtime").rglob("*.sock")), out / "wire-first-300.jsonl"
            )
            record["welcome"] = client.welcome
            if args.native_close:
                assert "session_close_v1" in client.welcome["features"]
            last_head = 0
            histories = {}
            for i in range(args.sessions + 4):
                if i == 0 or not args.reuse_session:
                    created = client.call(
                        "session.create",
                        command_id=f"create-{i}",
                        cwd=str(root),
                        provider="fake",
                        model="fake-model",
                        max_tokens=4096,
                    )
                    sid = created["session_id"]
                    generation = created["worker_generation"]
                    last_head = 0
                attached = client.call(
                    "session.attach",
                    session_id=sid,
                    after_seq=last_head,
                    mode="control",
                )
                attachment = attached["attachment_id"]
                submitted = client.call(
                    "turn.submit",
                    session_id=sid,
                    worker_generation=generation,
                    command_id=f"turn-{i}",
                    text=f"needle-{i}",
                    attachments=[],
                    mode="queue",
                )
                saw_done = False
                while True:
                    frame = client.event()
                    if frame.get("kind") == "event":
                        payload = frame["envelope"]["payload"]
                        if payload.get("type") == "run_state":
                            assert payload["state"] not in [
                                "errored",
                                "cancelled",
                            ], frame
                            saw_done |= (
                                payload["state"] == "done"
                                and frame["envelope"].get("run_id")
                                == submitted["run_id"]
                            )
                        if (
                            saw_done
                            and payload.get("type") == "session_state"
                            and (payload["state"] == "idle")
                        ):
                            break
                # accepted_seq names the user message, after the queued-run
                # fact. Read from the prior durable head to retain every fact.
                start_seq = last_head + 1 if args.reuse_session else 1
                turn_range = dict(start_seq=start_seq, end_seq=start_seq + 255)
                before = client.call("session.read", session_id=sid, range=turn_range)
                assert "retention-needle" in json.dumps(before), before
                fields = dict(attachment_id=attachment)
                if args.native_close:
                    fields["close_session"] = True
                closed = client.call("session.detach", **fields)
                if args.native_close:
                    assert closed["closed_session_id"] == sid, closed
                after = client.call("session.read", session_id=sid, range=turn_range)
                assert before == after, "close changed durable history"
                assert any(
                    envelope["payload"].get("type") == "user_message"
                    and envelope["payload"].get("text") == f"needle-{i}"
                    for envelope in after["result"]["envelopes"]
                ), "unique user needle was lost"
                history = histories.setdefault(sid, {})
                for envelope in after["result"]["envelopes"]:
                    history[envelope["seq"]] = envelope_digest(envelope)
                last_head = after["result"]["head_seq"]
                with (out / "receipts.jsonl").open("a") as receipts:
                    receipts.write(
                        json.dumps(
                            dict(
                                ordinal=i,
                                session_id=sid,
                                submitted=submitted,
                                closed=closed,
                                history_sha256=hashlib.sha256(
                                    json.dumps(before, sort_keys=True).encode()
                                ).hexdigest(),
                            )
                        )
                        + "\n"
                    )
                client.events.clear()
                if i == 3 or (i - 3) % 100 == 0 or i == args.sessions + 3:
                    record["snapshots"].append(
                        resources(args.sampler, daemon.pid, f"after-{max(0, i - 3)}")
                    )
                    (out / "result.json").write_text(
                        json.dumps(record, indent=2) + "\n"
                    )
                if i == 3:
                    time.sleep(15)
                    record["snapshots"].append(
                        resources(args.sampler, daemon.pid, "warm-settled-15s")
                    )
            for seconds in [5, 15]:
                time.sleep(5 if seconds == 5 else 10)
                record["snapshots"].append(
                    resources(args.sampler, daemon.pid, f"settled-{seconds}s")
                )
            if args.native_close:
                warm = next(
                    (s for s in record["snapshots"] if s["label"] == "warm-settled-15s")
                )
                settled = record["snapshots"][-1]
                assert settled["fd_count"] <= warm["fd_count"], (warm, settled)
                for snapshot in record["snapshots"]:
                    assert snapshot["fd_count"] <= warm["fd_count"], snapshot
                    assert not any(
                        (fd["path"].endswith(".pipe") for fd in snapshot["fds"])
                    ), snapshot
                assert settled["thread_count"] <= warm["thread_count"], (warm, settled)
            # Do this after the timed resource census so the extra verification
            # reads cannot extend the 15-second blocking-worker settle window.
            record["durable_envelopes_verified"] = verify_preserved_histories(
                client, histories
            )
            record["journal_needle_survival"] = 1.0
            client.call("daemon.shutdown")
            client.socket.close()
            client = None
            daemon.wait(timeout=60)
            record["daemon_exit_code"] = daemon.returncode
            assert daemon.returncode == 0, daemon.returncode
            record["verdict"] = "PASS"
        except BaseException as error:
            record["verdict"] = "FAIL"
            record["error"] = repr(error)
            raise
        finally:
            if client is not None:
                client.socket.close()
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()
            if not args.retain_profile:
                shutil.rmtree(root)
            record["final_daemon_exit_code"] = daemon.returncode
            (out / "result.json").write_text(json.dumps(record, indent=2) + "\n")
            print(
                json.dumps(
                    {
                        k: record.get(k)
                        for k in [
                            "verdict",
                            "error",
                            "pid",
                            "binary_sha256",
                            "final_daemon_exit_code",
                        ]
                    }
                )
            )


if __name__ == "__main__":
    main()
