#!/usr/bin/env python3
"""Connection result ordering regression, using local fake hosts only.

Run after building haider, haiderd and haider-tui, inside a build-slot ticket:
  python terminal-result-order.py --bin-dir <target/debug> --evidence <output>
Modes:
  (default)            current peer, ample limits, every result shape;
  --legacy-peer-limit  text-fallback compatibility (proxied): the real tight-limit
                       Welcome on an ample transport, with an explicit adapter that
                       models the older setup-result wire (setup_error stripped,
                       setup text synthesised as terminal output);
  --tight-peer-limit   real tight-limit peer: the TUI's own Hello carries the
                       previous Welcome ceiling, so the daemon withholds
                       ssh_typed_setup_errors_v1 on that connection, and every
                       daemon frame reaches the TUI byte for byte (no rewrite).
Requires tmux, paramiko and cryptography. All profiles are disposable; no provider
or existing connection settings are used. Captures are actual tmux text/ANSI.
"""
from pathlib import Path
import argparse
import base64
import copy
import io
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
import paramiko
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization

signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
args = argparse.ArgumentParser(
    description="Exercise connection result ordering in actual CLI/TUI binaries with local fake hosts."
)
args.add_argument("--bin-dir", type=Path, required=True)
args.add_argument("--evidence", type=Path, required=True)
mode_group = args.add_mutually_exclusive_group()
mode_group.add_argument(
    "--legacy-peer-limit", action="store_true",
    help="Text-fallback compatibility (proxied): previous Welcome features with a modelled legacy setup-result wire.",
)
mode_group.add_argument(
    "--tight-peer-limit", action="store_true",
    help="Real tight-limit peer: the TUI's Hello carries the previous ceiling; daemon frames are relayed unmodified.",
)
config = args.parse_args()
TOKEN = "ssh_typed_setup_errors_v1"
B = config.bin_dir.resolve()
E = config.evidence.resolve()
E.mkdir(parents=True, exist_ok=True)
L = E / "live"
L.mkdir(exist_ok=True)
(E / "tmux-fixture.conf").write_text('set -g default-terminal "xterm-256color"\n')

SB = E / "loopback-only.sb"
SB.write_text(
    '(version 1)\n(allow default)\n(deny network-outbound (remote ip "*:*"))\n(allow network-outbound (remote ip "localhost:*"))\n'
)


def wait(pred, label, timeout=30):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = pred()
        if value:
            return value
        time.sleep(0.03)
    raise AssertionError("timeout: " + label)


def read(s, n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise EOFError()
        data += chunk
    return data


def receive_raw(s):
    body = read(s, struct.unpack(">I", read(s, 4))[0])
    return body, json.loads(body)


def receive(s):
    return receive_raw(s)[1]


def raw_frame(body):
    return struct.pack(">I", len(body)) + body


def encode(d):
    data = json.dumps(d, separators=(",", ":")).encode()
    return struct.pack(">I", len(data)) + data


def omit_typed_setup_fields(value):
    if isinstance(value, dict):
        value.pop("setup_error", None)
        for child in value.values():
            omit_typed_setup_fields(child)
    elif isinstance(value, list):
        for child in value:
            omit_typed_setup_fields(child)


class Proxy:
    def __init__(self, path, real):
        self.path = path
        self.real = real
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.server = socket.socket(socket.AF_UNIX)
        self.server.bind(str(path))
        os.chmod(path, 0o600)
        self.server.listen()
        self.server.settimeout(0.2)
        self.stop = False
        self.peers = []
        self.connections = []
        self.tui = None
        self.trace = []
        self.hold_next = False
        self.held = []
        self.opens = []
        self.admissions = {}
        self.closes = []
        self.submits = []
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        while not self.stop:
            try:
                client, _ = self.server.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            upstream = socket.socket(socket.AF_UNIX)
            upstream.connect(str(self.real))
            c = {
                "client": client,
                "upstream": upstream,
                "lock": threading.RLock(),
                "tui": False,
            }
            self.connections.append(c)
            self.peers.extend([client, upstream])
            threading.Thread(target=self.relay, args=(c, True), daemon=True).start()
            threading.Thread(target=self.relay, args=(c, False), daemon=True).start()

    def send(self, c, frame, raw=None):
        with c["lock"]:
            c["client"].sendall(raw_frame(raw) if raw is not None else encode(frame))

    def tight_hello(self, c, frame):
        # Discover this disposable daemon's own replies, then present the TUI's
        # actual Hello with the largest receive limit at which the daemon must
        # withhold the new token: exactly the previous Welcome's byte length.
        with socket.socket(socket.AF_UNIX) as probe:
            probe.connect(str(self.real))
            probe.sendall(encode(frame))
            full_raw, full = receive_raw(probe)
        assert TOKEN in full["features"], "ample peer receives the new token"
        below_full = copy.deepcopy(frame)
        below_full["max_receive_frame"] = len(full_raw) - 1
        with socket.socket(socket.AF_UNIX) as probe:
            probe.connect(str(self.real))
            probe.sendall(encode(below_full))
            tight_raw, tight = receive_raw(probe)
        assert set(full["features"]) - set(tight["features"]) == {TOKEN}, "only the new token is withheld"
        assert set(tight["features"]) <= set(full["features"])
        c["tight_limit"] = len(tight_raw)
        c["tight_probe_raw"] = tight_raw
        frame["max_receive_frame"] = c["tight_limit"]
        self.trace.append({
            "kind": "TUI Hello sent with previous Welcome ceiling",
            "full_welcome_bytes": len(full_raw),
            "max_receive_frame": c["tight_limit"],
            "withheld": [TOKEN],
        })

    def legacy_wire(self, c, frame):
        if frame.get("kind") == "welcome":
            # General TUI startup messages can exceed the old Welcome ceiling.
            # Supply the actual separately negotiated tight-limit Welcome while
            # keeping an ample transport for the remaining application messages.
            assert "ssh_typed_setup_errors_v1" in frame["features"]
            frame.clear()
            frame.update(copy.deepcopy(c["legacy_welcome"]))
            assert "ssh_typed_setup_errors_v1" not in frame["features"]
            self.trace.append({
                "kind": "new capability withheld by daemon",
                "welcome": copy.deepcopy(frame),
            })
        # The feature list comes from the actual tight-limit handshake. This
        # fixture explicitly models the older setup-result wire shape.
        shell = frame.get("shell", {})
        error = shell.get("setup_error")
        if frame.get("kind") == "shell.state" and error:
            text = f"{error['code']}: {error['message']}\r\n"
            legacy = {
                "v": 1,
                "kind": "shell.output",
                "id": shell["id"],
                "stream": "stderr",
                "chunk_b64": base64.b64encode(text.encode()).decode(),
            }
            self.trace.append({
                "kind": "legacy wire fixture from actual setup result",
                "source": copy.deepcopy(shell),
                "frame": copy.deepcopy(legacy),
            })
            self.send(c, legacy)
        omit_typed_setup_fields(frame)

    def relay(self, c, outgoing):
        try:
            while not self.stop:
                raw, frame = receive_raw(c["client"] if outgoing else c["upstream"])
                if outgoing:
                    if frame.get("kind") == "hello":
                        c["tui"] = frame.get("client_kind") == "tui"
                        if c["tui"]:
                            self.tui = c
                            if config.tight_peer_limit:
                                self.tight_hello(c, frame)
                            if config.legacy_peer_limit:
                                # Discover this disposable daemon's identity fields,
                                # then request its exact pre-capability byte ceiling.
                                with socket.socket(socket.AF_UNIX) as probe:
                                    probe.connect(str(self.real))
                                    probe.sendall(encode(frame))
                                    previous = receive(probe)
                                previous["features"].remove("ssh_typed_setup_errors_v1")
                                tight_hello = copy.deepcopy(frame)
                                tight_hello["max_receive_frame"] = len(encode(previous)) - 4
                                with socket.socket(socket.AF_UNIX) as probe:
                                    probe.connect(str(self.real))
                                    probe.sendall(encode(tight_hello))
                                    actual = receive(probe)
                                assert actual == previous, "actual old-limit Welcome"
                                c["legacy_welcome"] = actual
                                self.trace.append({
                                    "kind": "previous Welcome ceiling requested",
                                    "limit": tight_hello["max_receive_frame"],
                                    "actual_reply": copy.deepcopy(actual),
                                    "previous_reply_preserved": True,
                                    "tui_transport": "original ample receive limit",
                                })
                    body = frame.get("body", {})
                    method = body.get("method")
                    if c["tui"] and method:
                        if method == "ssh.shell_open":
                            self.opens.append(
                                {
                                    "request_id": frame["request_id"],
                                    "profile": body.get("name"),
                                }
                            )
                            if self.hold_next:
                                self.hold_next = False
                                c["hold_id"] = frame["request_id"]
                        if method == "shell.close":
                            self.closes.append(body["id"])
                        if method == "turn.submit":
                            self.submits.append(copy.deepcopy(body))
                            self.trace.append(
                                {
                                    "kind": "turn submitted from real TUI",
                                    "monotonic": time.monotonic(),
                                    "body": copy.deepcopy(body),
                                }
                            )
                    c["upstream"].sendall(encode(frame))
                else:
                    with c["lock"]:
                        if c["tui"]:
                            if config.legacy_peer_limit:
                                self.legacy_wire(c, frame)
                            if config.tight_peer_limit:
                                c["max_relayed"] = max(c.get("max_relayed", 0), len(raw))
                                if frame.get("kind") == "welcome":
                                    self.trace.append({
                                        "kind": "actual tight-limit Welcome relayed unmodified",
                                        "bytes": len(raw),
                                        "limit": c["tight_limit"],
                                        "token_present": TOKEN in frame.get("features", []),
                                        "identical_to_probe": raw == c["tight_probe_raw"],
                                        "welcome": copy.deepcopy(frame),
                                    })
                            if frame.get("kind") in [
                                "shell.opened",
                                "shell.state",
                                "shell.closed",
                                "shell.output",
                            ]:
                                self.trace.append(
                                    {
                                        "kind": "terminal event reached TUI proxy",
                                        "monotonic": time.monotonic(),
                                        "frame": copy.deepcopy(frame),
                                    }
                                )
                            if frame.get("body", {}).get("method") == "ssh.shell_open":
                                self.admissions[frame["request_id"]] = copy.deepcopy(
                                    frame["body"]["shell"]
                                )
                                self.trace.append(
                                    {
                                        "kind": "admission reply reached proxy",
                                        "request_id": frame.get("request_id"),
                                        "shell": copy.deepcopy(frame["body"]["shell"]),
                                    }
                                )
                            if (
                                "hold_id" in c
                                and frame.get("request_id") == c["hold_id"]
                            ):
                                self.held.append(
                                    (c, copy.deepcopy(frame), raw if config.tight_peer_limit else None)
                                )
                                c.pop("hold_id", None)
                                continue
                        # Tight mode forwards the daemon's exact bytes.
                        self.send(c, frame, raw if config.tight_peer_limit else None)
        except (EOFError, OSError):
            return

    def release(self):
        c, frame, raw = self.held.pop(0)
        shell = copy.deepcopy(frame.get("body", {}).get("shell"))
        self.trace.append(
            {"kind": "released delayed admission reply", "reply": copy.deepcopy(frame)}
        )
        self.send(c, frame, raw)
        return shell

    def close(self):
        self.stop = True
        self.server.close()
        for s in self.peers:
            try:
                s.close()
            except OSError:
                pass


ACTIVE = []


class Profile:
    def __init__(self, tag, size, ssh_port=None):
        self.tag = tag
        self.size = size
        self.sock = f"tv973-result-{os.getpid()}-{tag}"
        self.path = Path("/tmp") / (
            "tv973-" + str(os.getpid()) + "-" + str(time.time_ns() % 1000000)
        )
        self.path.mkdir(mode=0o700)
        for name in ["home", "profile", "rt", "ws"]:
            (self.path / name).mkdir(mode=0o700)
        self.env = {
            "PATH": str(B) + ":/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": str(self.path / "home"),
            "USERPROFILE": str(self.path / "home"),
            "HAIDER_PROFILE_DIR": str(self.path / "profile"),
            "HAIDER_RUNTIME_DIR": str(self.path / "rt"),
            "XDG_RUNTIME_DIR": str(self.path / "rt"),
            "HAIDER_DISCOVERY_DISABLED": "1",
            "HAIDER_NO_UPDATE_CHECK": "1",
            "HAIDER_TEST_DEVICE_NAME": "test-mac",
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "LANG": "en_US.UTF-8",
            "HAIDER_TEST_FAKE_PROVIDER": json.dumps(
                [
                    {"step": "emit_text", "text": "LIVE_READY"},
                    {"step": "finish", "reason": "end_turn"},
                ]
                * 4
            ),
        }
        self.proxy = None
        self.daemon = None
        self.log = None
        ACTIVE.append(self)
        self.log = (L / (tag + "-daemon.log")).open("w")
        self.daemon = subprocess.Popen(
            ["/usr/bin/sandbox-exec", "-f", str(SB), str(B / "haiderd")],
            env=self.env,
            cwd=self.path / "ws",
            stdout=self.log,
            stderr=subprocess.STDOUT,
        )
        endpoint = wait(
            lambda: next((self.path / "rt").rglob("h.sock"), None), "daemon socket"
        )
        client_root = self.path / "cr"
        client_root.mkdir(mode=0o700)
        self.proxy = Proxy(client_root / endpoint.parent.name / endpoint.name, endpoint)
        tui_env = self.env | {
            "HAIDER_RUNTIME_DIR": str(client_root),
            "XDG_RUNTIME_DIR": str(client_root),
        }
        assert self.daemon.poll() is None, "owned daemon stopped before TUI start"
        if ssh_port is not None:
            for name in ["fixture", "fixture-next"]:
                added = self.cli(
                    "ssh",
                    "add",
                    name,
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(ssh_port),
                    "--user",
                    "synthetic",
                    "--password-stdin",
                    input="synthetic-qa-password\n",
                )
                assert added.returncode == 0, added.stderr
        self.tm(
            "new-session",
            "-d",
            "-s",
            "live",
            "-x",
            str(size[0]),
            "-y",
            str(size[1]),
            "env",
            "-i",
            *[k + "=" + v for k, v in tui_env.items()],
            "/usr/bin/sandbox-exec",
            "-f",
            str(SB),
            str(B / "haider"),
            "tui",
            "--workspace",
            str(self.path / "ws"),
        )
        self.tm("set-option", "-g", "status", "off")
        self.tm("resize-window", "-t", "live:0", "-x", str(size[0]), "-y", str(size[1]))
        actual = self.tm(
            "display-message", "-p", "-t", "live:0", "#{pane_width}x#{pane_height}"
        ).strip()
        assert actual == f"{size[0]}x{size[1]}", actual
        self.until(lambda s: "[ IDLE ]" in s, "launcher")
        if config.tight_peer_limit:
            # A ~3 KiB transport cannot carry the session-creation replies of a
            # seed turn (33-sol-r11-cap2 first attempt); the SSH journey's
            # frames fit, so it runs on the launcher directly.
            return
        self.literal("seed live terminal")
        self.key("Enter")
        self.until(lambda s: "LIVE_READY" in s, "seed completion")
        time.sleep(0.5)
        assert any(
            x.get("text") == "seed live terminal" for x in self.proxy.submits
        ), "submit trace did not observe the seed turn"

    def tm(self, *args):
        return subprocess.check_output(
            ["tmux", "-f", str(E / "tmux-fixture.conf"), "-L", self.sock, *args],
            text=True,
        )

    def screen(self):
        return self.tm("capture-pane", "-t", "live:0", "-p", "-J")

    def until(self, pred, label):
        try:
            return wait(lambda: (s if pred(s := self.screen()) else None), label, 45)
        except AssertionError:
            try:
                self.capture(
                    "failure-" + "".join(c if c.isalnum() else "-" for c in label)[:60]
                )
            except Exception:
                pass
            raise

    def key(self, *keys, delay=0.5):
        self.tm("send-keys", "-t", "live:0", *keys)
        time.sleep(delay)

    def literal(self, text, delay=0.5):
        self.tm("send-keys", "-t", "live:0", "-l", text)
        time.sleep(delay)

    def capture(self, label):
        stem = self.tag + "-" + label
        txt = self.screen()
        (L / (stem + ".txt")).write_text(txt)
        (L / (stem + ".ansi")).write_text(
            self.tm("capture-pane", "-t", "live:0", "-p", "-e")
        )
        (L / (stem + ".json")).write_text(
            json.dumps(
                {
                    "pane_size": self.tm(
                        "display-message",
                        "-p",
                        "-t",
                        "live:0",
                        "#{pane_width}x#{pane_height}",
                    ).strip(),
                    "source": "actual TUI in private tmux pane",
                    "binary_dir": str(B),
                    "tag": self.tag,
                },
                indent=2,
            )
            + "\n"
        )
        return txt

    def cli(self, *args, input=None):
        return subprocess.run(
            ["/usr/bin/sandbox-exec", "-f", str(SB), str(B / "haider"), *args],
            env=self.env,
            cwd=self.path / "ws",
            input=input,
            text=True,
            capture_output=True,
            timeout=30,
        )

    def close(self):
        if self not in ACTIVE:
            return
        if self.proxy is not None:
            (L / (self.tag + "-proxy-trace.json")).write_text(
                json.dumps(
                    {
                        "trace": self.proxy.trace,
                        "opens": self.proxy.opens,
                        "closes": self.proxy.closes,
                        "submits": self.proxy.submits,
                    },
                    indent=2,
                )
                + "\n"
            )
        subprocess.run(["tmux", "-L", self.sock, "kill-server"], capture_output=True)
        stop = self.cli("daemon", "stop")
        (L / (self.tag + "-stop.log")).write_text(
            stop.stdout + stop.stderr + f"\nrc={stop.returncode}\n"
        )
        try:
            try:
                rc = self.daemon.wait(20) if self.daemon is not None else 0
            except subprocess.TimeoutExpired:
                self.daemon.terminate()
                try:
                    self.daemon.wait(5)
                except subprocess.TimeoutExpired:
                    self.daemon.kill()
                    self.daemon.wait()
                rc = 124
        finally:
            if self.proxy is not None:
                self.proxy.close()
            if self.log is not None:
                self.log.close()
            ACTIVE.remove(self)
            shutil.rmtree(self.path)
        assert stop.returncode == 0 and rc == 0, (stop.returncode, rc)


class Fixture:
    def __init__(self, connected=False):
        self.shell_started = threading.Event()
        self.connected = connected
        self.stop = threading.Event()
        self.peers = []
        self.transports = []
        self.bytes = []
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.sock.listen()
        self.sock.settimeout(0.2)
        pem = Ed25519PrivateKey.generate().private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.OpenSSH,
            serialization.NoEncryption(),
        )
        self.key = paramiko.Ed25519Key.from_private_key(io.StringIO(pem.decode()))
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while not self.stop.is_set():
            try:
                c, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            self.peers.append(c)
            if self.connected:
                threading.Thread(target=self.ssh, args=(c,), daemon=True).start()

    def close(self):
        self.stop.set()
        self.sock.close()
        for t in self.transports:
            t.close()
        for c in self.peers:
            try:
                c.close()
            except OSError:
                pass


class LocalTerminal(Fixture):
    def __init__(self, mode):
        self.mode = mode
        self.gate = threading.Event()
        super().__init__(connected=True)

    def ssh(self, c):
        self.gate.wait()
        if self.stop.is_set():
            return
        if self.mode == "failed-setup":
            c.close()
            return
        fixture = self

        class LocalAuth(paramiko.ServerInterface):
            def get_allowed_auths(self, username):
                return "password"

            def check_auth_password(self, username, password):
                return (
                    paramiko.AUTH_SUCCESSFUL
                    if username == "synthetic" and password == "synthetic-qa-password"
                    else paramiko.AUTH_FAILED
                )

            def check_channel_request(self, *args):
                return paramiko.OPEN_SUCCEEDED

            def check_channel_pty_request(self, *args):
                return True

            def check_channel_shell_request(self, channel):
                def terminal():
                    fixture.shell_started.set()
                    try:
                        if fixture.mode == "early-output":
                            channel.sendall(b"EARLY_TERMINAL_OUTPUT\r\n")
                        while not fixture.stop.is_set():
                            data = channel.recv(32768)
                            if not data:
                                break
                            fixture.bytes.append(data.hex())
                            if b"\x03" in data:
                                channel.sendall(b"LOCAL_CTRL_C_RECEIVED\r\n")
                    except (OSError, EOFError):
                        pass

                threading.Thread(target=terminal, daemon=True).start()
                return True

        transport = paramiko.Transport(c)
        self.transports.append(transport)
        transport.add_server_key(self.key)
        try:
            transport.start_server(server=LocalAuth())
        except Exception:
            pass

    def close(self):
        self.gate.set()
        super().close()


rows = []
MODES = (
    ["failed-setup"]
    if config.tight_peer_limit
    else ["failed-setup", "silent-running", "early-output"]
)
for size in [(118, 36), (80, 24)]:
    for mode in MODES:
        for order in ["reply-first", "result-first"]:
            tag = f"terminal-order-{mode}-{order}-{size[0]}x{size[1]}"
            p = None
            fixture = LocalTerminal(mode)
            row = {
                "tag": tag,
                "mode": mode,
                "order": order,
                "size": list(size),
                "passed": False,
                "fixture": "Actual local fake terminal; only the connection reply is held. Status/output events continue to the real TUI.",
            }
            try:
                p = Profile(tag, size, ssh_port=fixture.port)
                p.literal("/ssh")
                p.key("Enter")
                p.until(lambda s: "fixture-next" in s, "terminal profile list")
                time.sleep(0.5)
                p.proxy.hold_next = True
                p.key("Enter", delay=0.3)
                wait(lambda: p.proxy.held, "connection reply held")
                shell = p.proxy.held[0][1]["body"]["shell"]
                shell_id = shell["id"]
                row["shell_id"] = shell_id
                if order == "reply-first":
                    p.proxy.release()
                    time.sleep(0.6)
                fixture.gate.set()

                def events():
                    return [
                        x["frame"]
                        for x in p.proxy.trace
                        if x.get("kind") == "terminal event reached TUI proxy"
                    ]

                def terminal_state():
                    return next(
                        (
                            x["shell"]
                            for x in reversed(events())
                            if x.get("shell", {}).get("id") == shell_id
                            and x["shell"]["status"]["status"]
                            in (
                                ["exited", "closed"]
                                if mode == "failed-setup"
                                else ["running"]
                            )
                        ),
                        None,
                    )

                state = wait(terminal_state, "actual terminal result", 45)
                row["actual_terminal_status"] = state["status"]
                row["actual_setup_code"] = (state.get("setup_error") or {}).get("code")
                if mode == "early-output":
                    wait(
                        lambda: any(
                            x.get("kind") == "shell.output" and x.get("id") == shell_id
                            for x in events()
                        ),
                        "actual early terminal output",
                    )
                time.sleep(0.5)
                p.capture(
                    "result-before-delayed-reply"
                    if order == "result-first"
                    else "result-after-reply"
                )
                if order == "result-first":
                    p.proxy.release()
                    time.sleep(0.8)
                frame = p.capture("after-both-arrived")
                row["legacy_peer_limit"] = config.legacy_peer_limit
                row["tight_peer_limit"] = config.tight_peer_limit
                if config.legacy_peer_limit:
                    row["journey_label"] = "text-fallback compatibility (proxied)"
                    row["legacy_transport"] = (
                        "Actual tight-limit Welcome; ample subsequent transport; "
                        "modelled older setup-result wire"
                    )
                if config.tight_peer_limit:
                    row["journey_label"] = "real tight-limit peer (daemon frames unmodified)"
                    welcomes = [
                        x for x in p.proxy.trace
                        if x["kind"] == "actual tight-limit Welcome relayed unmodified"
                    ]
                    tui_conns = [x for x in p.proxy.connections if x["tui"]]
                    row["tui_connections"] = len(tui_conns)
                    row["tight_limit"] = p.proxy.tui["tight_limit"]
                    row["max_frame_relayed_to_tui"] = max(
                        x.get("max_relayed", 0) for x in tui_conns
                    )
                    row["welcome_token_present"] = any(x["token_present"] for x in welcomes)
                    row["welcome_identical_to_probe"] = bool(welcomes) and all(
                        x["identical_to_probe"] for x in welcomes
                    )
                    row["actual_setup_error_delivered"] = bool(row["actual_setup_code"])
                    row["setup_text_as_output"] = any(
                        x.get("kind") == "shell.output" and x.get("id") == shell_id
                        for x in events()
                    )
                if config.legacy_peer_limit:
                    assert any(
                        x["kind"] == "new capability withheld by daemon"
                        for x in p.proxy.trace
                    )
                row["opening_still_visible"] = (
                    "opening remote terminal" in frame.lower()
                )
                row["output_visible"] = "EARLY_TERMINAL_OUTPUT" in frame
                if mode == "failed-setup":
                    row["typed_result_visible"] = bool(
                        row["actual_setup_code"] and row["actual_setup_code"] in frame
                    )
                    row["legacy_result_visible"] = "ssh_connection_failed" in frame
                    row["passed"] = (
                        not row["opening_still_visible"]
                        and (
                            row["legacy_result_visible"]
                            if config.legacy_peer_limit
                            else row["typed_result_visible"]
                        )
                    )
                    if config.tight_peer_limit:
                        row["passed"] = (
                            row["passed"]
                            and row["actual_setup_error_delivered"]
                            and not row["setup_text_as_output"]
                            and not row["welcome_token_present"]
                            and row["welcome_identical_to_probe"]
                            and "remote terminal failed: " + row["actual_setup_code"] in frame.replace("\n", "")
                        )
                else:
                    p.key("C-c", delay=0.8)
                    p.capture("after-control-c")
                    row["control_c_forwarded"] = "03" in fixture.bytes
                    row["current_shell_closed_by_tui"] = shell_id in p.proxy.closes
                    row["passed"] = (
                        not row["opening_still_visible"]
                        and row["control_c_forwarded"]
                        and not row["current_shell_closed_by_tui"]
                        and (mode != "early-output" or row["output_visible"])
                    )
            except Exception as error:
                row["harness_error"] = repr(error)
                if p is not None:
                    try:
                        p.capture("driver-error")
                    except Exception:
                        pass
            finally:
                fixture.close()
                if p is not None:
                    try:
                        p.close()
                    except Exception as error:
                        row["cleanup_error"] = repr(error)
                        row["passed"] = False
                for active in list(ACTIVE):
                    try:
                        active.close()
                    except Exception as error:
                        row["cleanup_error"] = repr(error)
                        row["passed"] = False
                rows.append(row)
                (E / "terminal-result-order-live.json").write_text(
                    json.dumps(rows, indent=2) + "\n"
                )
                print(json.dumps(row), flush=True)
assert len(rows) == 4 * len(MODES)
sys.exit(0 if all(r["passed"] for r in rows) else 1)
