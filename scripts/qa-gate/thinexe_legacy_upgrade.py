#!/usr/bin/env python3
"""Black-box the frozen 969 updater against a local, trusted HTTPS fixture.

No binary patching, host/proxy setting changes, DNS changes, system trust edits,
or public network forwarding. Only child-process proxy/CA variables are set.
The historical executable owns archive acceptance, staging, commit and restart.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socketserver
import ssl
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from typing import Any, Callable, Iterator
from urllib.parse import urlsplit

from gate.contract import (
    DAEMON_STARTUP, DAEMON_STOP, PROCESS_EXIT_GRACE, STATUS_REQUEST, VERSION_QUERY,
)
from turnperf_support import (
    ProofError, parse_single_json, profile_daemon_pid, profile_lock_is_free,
    sha256_file, wait_pid_gone,
)

NAMES = ("haider", "haider-tui", "haiderd")
CURL = "/usr/bin/curl"
# Registry #94: one release-list request + two downloads each inherit curl's
# 120s --max-time. Each old member has up to four xattr/signature commands,
# a version probe, then signature+version installed probes; assign the existing
# cold-version allowance to each. The CLI self-test gets the request allowance.
# Restart wraps 20s drain + 20s lock release + 30s health, then exit observation.
UPDATE_TIMEOUT = (3 * 120 + 2 * (4 + 1 + 2) * VERSION_QUERY.seconds
                  + STATUS_REQUEST.seconds + 20 + 20 + 30 + PROCESS_EXIT_GRACE.seconds)
# Three-member migration has the same per-member checks, one CLI self-test,
# then the forwarded status RPC. It has no network request or daemon restart.
MIGRATION_TIMEOUT = (3 * (4 + 1 + 2) * VERSION_QUERY.seconds
                     + 2 * STATUS_REQUEST.seconds + PROCESS_EXIT_GRACE.seconds)
# The live-daemon case must survive archive verification before the updater
# connects: cover update + migration + final status/stop observation, below the
# existing one-hour production maximum. This does not avoid969's signal/linger
# escalation: launcher exit still records the first shutdown demand.
LIVE_FIXTURE_IDLE_TTL_MS = int((UPDATE_TIMEOUT + MIGRATION_TIMEOUT + STATUS_REQUEST.seconds
                              + DAEMON_STOP.seconds + PROCESS_EXIT_GRACE.seconds) * 1000)


class TransportBlocked(ProofError):
    pass


def fingerprint(path: Path) -> dict[str, Any]:
    stat = path.stat()
    return {"sha256": sha256_file(path), "bytes": stat.st_size,
            "mode": stat.st_mode & 0o777, "inode": stat.st_ino}


def members_snapshot(directory: Path) -> dict[str, Any]:
    return {name: fingerprint(directory / name) if (directory / name).exists() else None
            for name in NAMES}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ProofError(message)


def child_environment(root: Path, proxy: str, ca: Path) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("HAIDER_")
           and not key.endswith(("_API_KEY", "_TOKEN", "_SECRET"))
           and not key.lower().endswith("_proxy")
           and key not in {"CURL_CA_BUNDLE", "SSL_CERT_FILE", "SSL_CERT_DIR"}}
    for name in ("home", "profile", "runtime", "workspace"):
        (root / name).mkdir(parents=True, mode=0o700, exist_ok=True)
    env.update(HOME=str(root / "home"), USERPROFILE=str(root / "home"),
               XDG_CONFIG_HOME=str(root / "home" / ".config"),
               XDG_DATA_HOME=str(root / "home" / ".local" / "share"),
               XDG_CACHE_HOME=str(root / "home" / ".cache"),
               HAIDER_PROFILE_DIR=str(root / "profile"),
               HAIDER_RUNTIME_DIR=str(root / "runtime"),
               HAIDER_DISCOVERY_DISABLED="1", HAIDER_NO_UPDATE_CHECK="1",
               HAIDER_RUN_DAEMON_IDLE_TTL_MS=str(LIVE_FIXTURE_IDLE_TTL_MS),
               HAIDER_TEST_DEVICE_NAME="test-mac", TMPDIR=str(root),
               HTTPS_PROXY=proxy, https_proxy=proxy, NO_PROXY="", no_proxy="",
               CURL_CA_BUNDLE=str(ca), SSL_CERT_FILE=str(ca), NO_COLOR="1")
    return env


def command(argv: list[str], env: dict[str, str], cwd: Path, timeout: float,
            ledger: list[dict[str, Any]]) -> dict[str, Any]:
    started = time.monotonic()
    process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        stdout, stderr = process.communicate()
    row = {"argv": argv, "pid": process.pid, "returncode": process.returncode,
           "timed_out": timed_out,
           "elapsed_seconds": time.monotonic() - started,
           "stdout": stdout.decode("utf-8", errors="replace"),
           "stderr": stderr.decode("utf-8", errors="replace")}
    ledger.append(row)
    require(not timed_out, f"command exceeded derived {timeout}s deadline: {argv}")
    return row


def success(row: dict[str, Any], operation: str) -> None:
    require(row["returncode"] == 0,
            f"{operation} failed exit={row['returncode']}: {row['stderr']}")


def create_certificate(directory: Path) -> tuple[Path, Path]:
    config = directory / "openssl.cnf"
    config.write_text("""[req]
distinguished_name = dn
x509_extensions = extensions
prompt = no
[dn]
CN = thinexe local fixture
[extensions]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:api.github.com,DNS:github.com
""", encoding="utf-8")
    cert, key = directory / "fixture-ca.pem", directory / "fixture-key.pem"
    result = subprocess.run(["/usr/bin/openssl", "req", "-x509", "-newkey", "rsa:2048",
                             "-nodes", "-days", "1", "-config", str(config),
                             "-keyout", str(key), "-out", str(cert)],
                            capture_output=True, text=True, timeout=VERSION_QUERY.seconds)
    if result.returncode:
        raise TransportBlocked(f"cannot generate subprocess-only fixture CA: {result.stderr}")
    key.chmod(0o600)
    return cert, key


class Fixture:
    def __init__(self, version: str, target: str, repository: str, archive: Path,
                 before_asset: Callable[[], None] | None = None):
        self.version, self.repository, self.archive = version, repository, archive
        self.top = f"haider-v{version}-{target}"
        self.archive_name = self.top + ".tar.xz"
        self.digest = sha256_file(archive)
        self.before_asset = before_asset
        self.requests: list[dict[str, Any]] = []
        self.errors: list[str] = []

    def response(self, host: str, path: str) -> tuple[str, bytes | Path]:
        parsed = urlsplit(path)
        if host == "api.github.com" and parsed.path == f"/repos/{self.repository}/releases":
            assets = [{"name": self.archive_name + suffix,
                       "browser_download_url": f"https://github.com/{self.repository}/releases/download/v{self.version}/{self.archive_name}{suffix}"}
                      for suffix in ("", ".sha256")]
            return "application/json", json.dumps([{
                "tag_name": "v" + self.version, "draft": False,
                "prerelease": False, "assets": assets,
            }]).encode()
        prefix = f"/{self.repository}/releases/download/v{self.version}/"
        if host == "github.com" and parsed.path in {prefix + self.archive_name,
                                                       prefix + self.archive_name + ".sha256"}:
            if self.before_asset:
                self.before_asset()
            if parsed.path.endswith(".sha256"):
                return "text/plain", f"{self.digest}  {self.archive_name}\n".encode()
            return "application/octet-stream", self.archive
        raise ProofError(f"unexpected fixture request: {host}{path}")


class FixtureHTTP(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self) -> None:
        fixture = self.server.fixture
        host = self.headers.get("Host", "").split(":")[0]
        row = {"host": host, "path": self.path}
        try:
            kind, body = fixture.response(host, self.path)
            row["status"] = 200
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(body.stat().st_size if isinstance(body, Path) else len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            if isinstance(body, Path):
                with body.open("rb") as source:
                    shutil.copyfileobj(source, self.wfile, 64 * 1024)
            else:
                self.wfile.write(body)
        except Exception as error:
            fixture.errors.append(str(error))
            row["status"] = 500
            self.send_error(500)
        finally:
            fixture.requests.append(row)
            self.close_connection = True

    def log_message(self, _format: str, *_args: Any) -> None:
        pass


class ConnectHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        try:
            self.request.settimeout(VERSION_QUERY.seconds)
            header = bytearray()
            # Read only CONNECT headers; never consume bytes from the TLS stream.
            while not header.endswith(b"\r\n\r\n"):
                part = self.request.recv(1)
                if not part:
                    return
                header.extend(part)
                require(len(header) <= 16 * 1024, "oversized CONNECT header")
            first = bytes(header).split(b"\r\n", 1)[0]
            require(first in {b"CONNECT api.github.com:443 HTTP/1.1",
                              b"CONNECT github.com:443 HTTP/1.1"},
                    f"fixture refuses forwarding: {first!r}")
            self.request.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
            with self.server.tls.wrap_socket(self.request, server_side=True) as connection:
                FixtureHTTP(connection, self.client_address, self.server)
        except Exception as error:
            self.server.fixture.errors.append(str(error))


class ProxyServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


@contextmanager
def proxy(fixture: Fixture, cert: Path, key: Path) -> Iterator[str]:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    with ProxyServer(("127.0.0.1", 0), ConnectHandler) as server:
        server.tls, server.fixture = context, fixture
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_address[1]}"
        finally:
            server.shutdown()
            thread.join()


def make_archive(path: Path, top: str, sources: dict[str, Path]) -> None:
    with tarfile.open(path, "w:xz", format=tarfile.USTAR_FORMAT) as archive:
        directory = tarfile.TarInfo(top + "/")
        directory.type, directory.mode = tarfile.DIRTYPE, 0o755
        archive.addfile(directory)
        for name, source in sources.items():
            info = tarfile.TarInfo(top + "/" + name)
            info.size, info.mode = source.stat().st_size, 0o755
            with source.open("rb") as stream:
                archive.addfile(info, stream)


def signed_reference(source: Path, destination: Path, *, historical: bool = True) -> dict[str, Any]:
    shutil.copy2(source, destination)
    destination.chmod(0o700)
    if not historical:
        valid = subprocess.run(["/usr/bin/codesign", "--verify", "--strict", str(destination)],
                               capture_output=True, text=True, timeout=VERSION_QUERY.seconds)
        if valid.returncode == 0:
            return fingerprint(destination)
    # The frozen969 updater always forces ad-hoc signing. The new directory
    # transaction preserves valid signatures and signs only unsigned images.
    force = ["--force"] if historical else []
    result = subprocess.run(["/usr/bin/codesign", *force, "--sign", "-",
                             "--timestamp=none", str(destination)], capture_output=True,
                            text=True, timeout=VERSION_QUERY.seconds)
    require(result.returncode == 0, f"reference ad-hoc signature failed: {result.stderr}")
    return fingerprint(destination)


def signature_references(sources: dict[str, Path], root: Path) -> dict[str, Any]:
    root.mkdir(mode=0o700)
    historical_dir, migration_dir = root / "historical", root / "migration"
    historical_dir.mkdir(mode=0o700)
    migration_dir.mkdir(mode=0o700)
    # codesign derives a default identifier from the destination basename.
    # The compatibility archive member is named haider, so a reference named
    # compat has different signed bytes despite identical executable code.
    # Separate phases also prevent the real thin haider overwriting that ref.
    historical = {
        "compat": signed_reference(sources["compat"], historical_dir / "haider"),
        "haiderd": signed_reference(sources["haiderd"], historical_dir / "haiderd"),
    }
    migration = {name: signed_reference(sources[name], migration_dir / name, historical=False)
                 for name in ("haider", "haider-tui")}
    migration["haiderd"] = historical["haiderd"]
    return {"historical": historical, "migration": migration}


def diagnostics(case: Path, install: Path) -> dict[str, Any]:
    """Retain exact transaction facts plus bounded log tails before cleanup."""
    marker = install / ".haider-update-transaction.json"
    result: dict[str, Any] = {"members": members_snapshot(install),
                              "transaction_assets": sorted(path.name for path in install.iterdir()
                                                           if path.name.startswith(".haider")),
                              "logs": []}
    if marker.is_file():
        raw = marker.read_bytes()
        result["marker"] = {"path": str(marker), "sha256": sha256_file(marker),
                            "raw": raw[:64 * 1024].decode("utf-8", errors="replace"),
                            "bytes": len(raw)}
        try:
            result["transaction_phase"] = json.loads(raw).get("phase")
        except (ValueError, AttributeError):
            pass
    paths = [case / "persistent-daemon.log", case / "profile" / "daemon.log"]
    paths.extend(sorted((case / "profile" / "daemon-logs").glob("*.log")))
    for path in paths:
        if path.is_file():
            with path.open("rb") as stream:
                size = path.stat().st_size
                stream.seek(max(0, size - 64 * 1024))
                tail = stream.read()
            result["logs"].append({"path": str(path), "bytes": size,
                                   "tail": tail.decode("utf-8", errors="replace")})
    return result


def run_independent_cases(report: dict[str, Any], exercise: Callable[[str, str, dict[str, Any]], None]) -> None:
    """A failed historical live-daemon case cannot suppress independent proof."""
    errors = []
    for label, mode in (("positive", "persistent"), ("historical_autospawn", "autospawn"),
                        ("negative", "none")):
        row = report.setdefault(label, {})
        row["daemon_start_mode"] = mode
        try:
            exercise(label, mode, row)
            require(row.get("passed") is True, f"{label} did not finish every assertion")
            row["status"] = "PASS"
        except Exception as error:
            row.update(passed=False, status="ENVIRONMENT-BLOCKED" if isinstance(error, TransportBlocked) else "FAIL",
                       error=str(error))
            errors.append(f"{label}: {error}")
    report.update(passed=not errors, status="PASS" if not errors else "FAIL")
    if errors:
        report["errors"] = errors
        report["error"] = "; ".join(errors)
        if all(report[label]["status"] == "ENVIRONMENT-BLOCKED"
               for label in ("positive", "historical_autospawn", "negative")):
            report["status"] = "ENVIRONMENT-BLOCKED"


def finish_scratch(root: Path, keep_scratch: bool, report: dict[str, Any]) -> None:
    # Failed cases retain their raw on-disk recovery state unconditionally.
    if not keep_scratch and report["passed"]:
        shutil.rmtree(root)
        report.pop("scratch", None)
    elif not report["passed"]:
        report["scratch_retained_for_failure"] = True


def transaction_clean(install: Path) -> None:
    leftovers = [path.name for path in install.iterdir()
                 if path.name.startswith(".haider") and path.name != ".haider-update.lock"]
    require(not leftovers, f"transaction assets were not cleaned: {leftovers}")


def status_document(row: dict[str, Any], profile: Path) -> dict[str, Any]:
    success(row, "status")
    document = parse_single_json(row["stdout"], "historical upgrade status")
    require(Path(document.get("profile_path", "")).resolve() == profile.resolve(),
            "status returned a different profile")
    require(isinstance(document.get("daemon", {}).get("pid"), int), "status lacks exact daemon PID")
    return document


def exercise_case(case: Path, baseline: Path, sources: dict[str, Path], references: dict[str, Any],
                  cert: Path, key: Path, version: str, target: str, repository: str,
                  negative: bool, report: dict[str, Any], *, daemon_mode: str = "autospawn") -> None:
    case.mkdir(mode=0o700)
    install = case / "bin"
    install.mkdir(mode=0o700)
    for name in ("haider", "haiderd"):
        shutil.copy2(baseline / name, install / name)
        (install / name).chmod(0o700)
    before = members_snapshot(install)
    report["before"] = before
    top = f"haider-v{version}-{target}"
    archive = case / (top + ".tar.xz")
    archive_sources = {"haider": sources["compat"], "haiderd": sources["haiderd"]}
    if negative:
        archive_sources["haider-tui"] = sources["haider-tui"]
    make_archive(archive, top, archive_sources)
    require(archive.stat().st_size <= 128 * 1024 * 1024, "fixture exceeds historical archive bound")
    def pre_transfer_check() -> None:
        require(members_snapshot(install) == before, "canonical members changed before verified download")
        require(not (install / ".haider-update.lock").exists(), "updater acquired lock before downloads completed")
        report.setdefault("pre_mutation_checks", []).append("canonical hashes/modes/inodes unchanged; no update lock")
    fixture = Fixture(version, target, repository, archive, pre_transfer_check)
    report["archive"] = {"sha256": sha256_file(archive), "members": list(archive_sources)}
    commands = report.setdefault("commands", [])
    with proxy(fixture, cert, key) as address:
        env = child_environment(case, address, cert)
        cli = str(install / "haider")
        retained: subprocess.Popen[bytes] | None = None
        retained_row: dict[str, Any] | None = None
        def run(args: list[str], timeout: float = STATUS_REQUEST.seconds) -> dict[str, Any]:
            return command([cli, *args], env, case / "workspace", timeout, commands)
        try:
            probe = command([CURL, "--fail", "--silent", "--show-error", "--max-time", "15",
                             f"https://api.github.com/repos/{repository}/releases?per_page=100&page=1"],
                            env, case, VERSION_QUERY.seconds, commands)
            if probe["returncode"] or not probe["stdout"].startswith("["):
                raise TransportBlocked(f"system curl did not honor isolated CA/proxy: {probe['stderr']}")
            old_status = None
            if not negative:
                if daemon_mode == "persistent":
                    log = case / "persistent-daemon.log"
                    argv = [str(install / "haiderd")]
                    with log.open("wb") as stream:
                        log.chmod(0o600)
                        retained = subprocess.Popen(argv, env={**env, "HAIDER_DAEMON_PROCESS_LOG": str(log)},
                                                    cwd=case / "workspace", stdin=subprocess.DEVNULL,
                                                    stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
                    retained_row = {"argv": argv, "pid": retained.pid, "log": str(log),
                                    "daemon_start_mode": "persistent"}
                    commands.append(retained_row)
                    # Direct969 haiderd has no launcher-liveness token. Probe
                    # readiness without auto-spawn under startup + one request.
                    deadline = time.monotonic() + DAEMON_STARTUP.seconds + STATUS_REQUEST.seconds
                    while True:
                        require(retained.poll() is None, "persistent969 daemon exited before readiness")
                        remaining = deadline - time.monotonic()
                        require(remaining > 0, "persistent969 daemon readiness deadline expired")
                        row = run(["status", "--json", "--no-spawn"], min(STATUS_REQUEST.seconds, remaining))
                        if row["returncode"] == 0:
                            old_status = status_document(row, case / "profile")
                            break
                        time.sleep(min(0.01, remaining))
                    require(old_status["daemon"]["pid"] == retained.pid, "status did not identify retained969 daemon")
                    require(old_status["daemon"].get("idle_ttl_ms") is None,
                            "persistent fixture unexpectedly has launcher idle policy")
                else:
                    success(run(["--ready"], DAEMON_STARTUP.seconds + STATUS_REQUEST.seconds), "old daemon readiness")
                    old_status = status_document(run(["status", "--json", "--no-spawn"]), case / "profile")
                    require(old_status["daemon"].get("idle_ttl_ms") == LIVE_FIXTURE_IDLE_TTL_MS,
                            "auto-spawn fixture did not retain its derived idle TTL")
                report["old_daemon"] = old_status["daemon"]
            update = run(["update"], UPDATE_TIMEOUT)
            report["after_update_diagnostics"] = diagnostics(case, install)
            require(not fixture.errors, f"HTTPS fixture errors: {fixture.errors}")
            require(len(report.get("pre_mutation_checks", [])) == 2,
                    "historical updater did not fetch both canonical archive and sidecar")
            if negative:
                require(update["returncode"] == 70, "legacy three-member archive was not refused with software exit70")
                require("unexpected or unsafe member" in update["stderr"],
                        "negative fixture did not reach the historical strict-member refusal")
                require(members_snapshot(install) == before, "legacy refusal mutated canonical bundle")
                require(not (install / ".haider-update.lock").exists(), "legacy refusal entered transaction")
                require(profile_daemon_pid(case / "profile") is None, "legacy refusal started a daemon")
            else:
                success(update, "historical updater")
                after_update = members_snapshot(install)
                report["after_legacy_update"] = after_update
                require(after_update["haider"]["sha256"] == references["historical"]["compat"]["sha256"],
                        "historical updater did not install exact signed compatibility entrypoint")
                require(after_update["haiderd"]["sha256"] == references["historical"]["haiderd"]["sha256"],
                        "historical updater did not install exact signed candidate daemon")
                require(after_update["haider-tui"] is None, "legacy updater unexpectedly published a payload")
                transaction_clean(install)
                old_pid = old_status["daemon"]["pid"]
                if retained is not None:
                    retained_row["returncode"] = retained.wait(timeout=PROCESS_EXIT_GRACE.seconds)
                require(wait_pid_gone(old_pid, PROCESS_EXIT_GRACE.seconds), "old daemon PID survived upgrade")
                new_pid = profile_daemon_pid(case / "profile")
                require(isinstance(new_pid, int) and new_pid != old_pid, "historical updater did not restart daemon")
                report["restarted_daemon_pid"] = new_pid
                # This normal headless verb must itself trigger compatibility
                # migration, then forward argv to the newly installed thin CLI.
                migrated = status_document(run(["status", "--json", "--no-spawn"], MIGRATION_TIMEOUT), case / "profile")
                report["migrated_daemon"] = migrated["daemon"]
                require(migrated["daemon"]["pid"] == new_pid, "compat migration restarted the already updated daemon")
                require(migrated["daemon"]["version"] == version, "restarted daemon version mismatch")
                after = members_snapshot(install)
                report["after_migration"] = after
                for name in NAMES:
                    require(after[name] is not None and after[name]["sha256"] == references["migration"][name]["sha256"],
                            f"migration did not publish exact signed {name} bytes")
                transaction_clean(install)
                version_row = run(["--version"], VERSION_QUERY.seconds)
                success(version_row, "migrated --version")
                require(version_row["stdout"] == f"haider {version}\n", "migrated CLI version mismatch")
                stop = run(["daemon", "stop", "--json"], DAEMON_STOP.seconds + PROCESS_EXIT_GRACE.seconds)
                success(stop, "updated daemon stop")
                receipt = parse_single_json(stop["stdout"], "updated daemon stop")
                require(receipt.get("daemon", {}).get("pid") == new_pid,
                        "stop receipt did not identify the restarted daemon")
                require(receipt.get("daemon", {}).get("process_exited") is True,
                        "stop receipt did not prove process exit")
                require(wait_pid_gone(new_pid, PROCESS_EXIT_GRACE.seconds), "updated daemon PID survived stop")
                report["stop_receipt"] = receipt
            require(profile_lock_is_free(case / "profile"), "profile lock remains held")
            transaction_clean(install)
            report["passed"] = True
        except Exception as error:
            report["error"] = str(error)
            raise
        finally:
            report["requests"], report["proxy_errors"] = fixture.requests, fixture.errors
            try:
                report["before_cleanup"] = diagnostics(case, install)
                if retained is not None:
                    retained.poll()  # Reap an exited retained child before kill-0 observation.
                owner = profile_daemon_pid(case / "profile")
                if owner and not wait_pid_gone(owner, PROCESS_EXIT_GRACE.seconds):
                    # Use the frozen thin control client: invoking a compatibility
                    # path could migrate it and overwrite recovery evidence.
                    try:
                        command([str(sources["haider"]), "daemon", "stop", "--json"], env,
                                case / "workspace", DAEMON_STOP.seconds + PROCESS_EXIT_GRACE.seconds,
                                commands)
                    except (OSError, ProofError):
                        pass
                    if retained is not None:
                        retained.poll()
                    if not wait_pid_gone(owner, PROCESS_EXIT_GRACE.seconds):
                        try:
                            os.kill(owner, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                        if retained is not None and owner == retained.pid:
                            retained.wait(timeout=DAEMON_STOP.seconds)
                        require(wait_pid_gone(owner, DAEMON_STOP.seconds), f"fixture daemon {owner} could not be cleaned up")
                if retained is not None:
                    if retained.poll() is None:
                        retained.terminate()
                    retained_row["returncode"] = retained.wait(timeout=DAEMON_STOP.seconds)
            except Exception as error:
                report["cleanup_error"] = str(error)
                if "error" not in report:
                    raise
            finally:
                report["after_cleanup"] = diagnostics(case, install)


def run(args: argparse.Namespace) -> dict[str, Any]:
    require(sys.platform == "darwin", "historical production updater only supports macOS; run on native macOS")
    target = "aarch64-apple-darwin" if platform.machine() == "arm64" else "x86_64-apple-darwin"
    sources = {name: (args.candidate / name).resolve() for name in NAMES}
    sources["compat"] = args.compat.resolve()
    inputs = {f"baseline/{name}": (args.baseline / name).resolve() for name in ("haider", "haiderd")}
    inputs.update({f"candidate/{name}": path for name, path in sources.items()})
    frozen = {name: {"path": str(path), **fingerprint(path)} for name, path in inputs.items()}
    report: dict[str, Any] = {"schema": "haider.thinexe.legacy-upgrade.v1", "passed": False,
                              "status": "FAIL", "inputs": frozen, "target": target,
                              "positive": {}, "historical_autospawn": {}, "negative": {}, "commands": [],
                              "limitations": "Native macOS correctness fixture, not performance evidence. Windows/Linux legacy self-update is unsupported. HTTPS is terminated locally; system trust/DNS/proxy settings are unchanged. Persistent-daemon migration and the historical auto-spawn drain limitation remain distinct assertions. Frozen969 staging forces ad-hoc signing; new directory staging preserves valid signatures, including the already re-signed daemon.",
                              "historical_source_observation": {
                                  "commit": "14750e31",
                                  "cause": "--ready exits after auto-spawn; request_after_idle sets requests=1, so the updater's first SIGTERM is treated as a repeated shutdown request and selects Forced",
                                  "references": ["crates/haider-daemon/src/lifecycle.rs:393", "crates/haider-daemon/src/lifecycle.rs:463", "crates/haider-cli/src/update/restart.rs:151"],
                                  "phase": "restart after canonical executable commit; not archive/staging refusal",
                              }}
    root = Path(tempfile.mkdtemp(prefix="htlu-", dir="/private/tmp"))
    report["scratch"] = str(root)
    try:
        cert, key = create_certificate(root)
        env = child_environment(root / "probe", "http://127.0.0.1:1", cert)
        for name, expected in (("baseline/haider", "haider 0.0.969\n"),
                               ("candidate/haider", f"haider {args.version}\n"),
                               ("candidate/haider-tui", f"haider-tui {args.version}\n"),
                               ("candidate/haiderd", f"haiderd {args.version}\n"),
                               ("candidate/compat", f"haider {args.version}\n")):
            row = command([str(inputs[name]), "--version"], env, root,
                          VERSION_QUERY.seconds, report["commands"])
            success(row, name + " identity")
            require(row["stdout"] == expected, f"unexpected {name} version: {row['stdout']!r}")
        references_dir = root / "references"
        references = signature_references(sources, references_dir)
        report["signed_references"] = references
        def exercise(label: str, mode: str, case_report: dict[str, Any]) -> None:
            exercise_case(root / label, args.baseline.resolve(), sources, references, cert, key,
                          args.version, target, args.repository, label == "negative", case_report,
                          daemon_mode=mode)
        run_independent_cases(report, exercise)
    except Exception as error:
        report["error"] = str(error)
        report["status"] = "ENVIRONMENT-BLOCKED" if isinstance(error, TransportBlocked) else "FAIL"
    finally:
        # Compare only artifact fields; path is report metadata, not a stat field.
        changed = [name for name, path in inputs.items()
                   if fingerprint(path) != {key: value for key, value in frozen[name].items() if key != "path"}]
        report["frozen_inputs_unchanged"] = not changed
        if changed:
            report.update(passed=False, status="FAIL", frozen_input_changes=changed)
        # Failure evidence includes recovery markers/backups and complete daemon
        # logs, not only a possibly truncated command error in the JSON report.
        finish_scratch(root, args.keep_scratch, report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, default=Path("/private/tmp/thinexe-before"))
    parser.add_argument("--candidate", type=Path, default=Path("target/release"))
    parser.add_argument("--compat", type=Path, default=Path("target/release/haider-compat"))
    parser.add_argument("--version", default="0.0.970")
    parser.add_argument("--repository", default="Rizzist/haider-agent")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--keep-scratch", action="store_true")
    args = parser.parse_args()
    try:
        report = run(args)
    except Exception as error:
        report = {"schema": "haider.thinexe.legacy-upgrade.v1", "passed": False,
                  "status": "FAIL", "error": str(error)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"{report['status']}: {args.output}")
    if report.get("error"):
        print(report["error"], file=sys.stderr)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
