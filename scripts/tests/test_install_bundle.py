#!/usr/bin/env python3
"""Offline wrapper boundaries; native executable transactions have a separate suite."""
from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[2]
VERSION = "9.8.7"
BINARIES = ("haider", "haider-tui", "haiderd")


def make_archive(root: Path, *, windows: bool, missing: str = "", binary_dir: Path | None = None, version: str = VERSION, spy: bool = False, wrong_payload: bool = False, legacy: bool = False) -> Path:
    targets = {
        ("Darwin", "arm64"): "aarch64-apple-darwin",
        ("Darwin", "x86_64"): "x86_64-apple-darwin",
        ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
        ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
    }
    target = "x86_64-pc-windows-msvc" if windows else targets[(platform.system(), platform.machine())]
    top = f"haider-v{version}-{target}" + ("" if legacy else "-split")
    names = [name for name in (('haider', 'haiderd') if legacy else BINARIES) if name != missing]
    if not windows and platform.system() == "Linux":
        names.append("haider-wayland-portal")
    artifact = root / (top + (".zip" if windows else ".tar.xz"))
    members = {}
    for name in names:
        filename = name + (".exe" if windows else "")
        if binary_dir is not None:
            source_name = ("haider" + (".exe" if windows else "")) if wrong_payload and name == "haider-tui" else filename
            members[filename] = (binary_dir / source_name).read_bytes()
        elif spy and name == "haider":
            members[filename] = (f"#!{sys.executable}\nimport json, os, pathlib, sys\npathlib.Path(os.environ['INSTALL_CALL']).write_text(json.dumps(sys.argv))\nsys.exit(int(os.environ.get('INSTALL_EXIT', '0')))\n").encode()
        else:
            members[filename] = f"new-{name}".encode()
    if windows:
        with zipfile.ZipFile(artifact, "w", zipfile.ZIP_STORED) as archive:
            for filename, data in members.items():
                archive.writestr(f"{top}/{filename}", data)
    else:
        with tarfile.open(artifact, "w:xz", preset=0) as archive:
            for filename, data in members.items():
                info = tarfile.TarInfo(f"{top}/{filename}")
                info.mode = 0o755
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
    digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
    (root / (artifact.name + ".sha256")).write_text(f"{digest}  {artifact.name}\n")
    return artifact


def old_install(prefix: Path, windows: bool) -> dict[str, bytes]:
    prefix.mkdir(parents=True)
    files = {}
    for name in BINARIES:
        filename = name + (".exe" if windows else "")
        files[filename] = f"old-{name}".encode()
        (prefix / filename).write_bytes(files[filename])
        (prefix / filename).chmod(0o700)
    return files


def contents(prefix: Path) -> dict[str, bytes]:
    return {p.name: p.read_bytes() for p in prefix.iterdir() if p.is_file()}


def fake_download_environment(root: Path, prefix: Path, version: str) -> dict[str, str]:
    commands = root / "commands"
    commands.mkdir(exist_ok=True)
    curl = commands / "curl"
    curl.write_text(f"#!{sys.executable}\nimport os, pathlib, sys\np = pathlib.Path(os.environ['INSTALL_FIXTURE']) / sys.argv[-1].rsplit('/', 1)[-1]\nsys.stdout.buffer.write(p.read_bytes())\n")
    curl.chmod(0o755)
    return dict(os.environ, PATH=str(commands) + os.pathsep + os.environ["PATH"], INSTALL_FIXTURE=str(root), HAIDER_INSTALL_DIR=str(prefix), HAIDER_VERSION=version)


class DownloadBudgetTests(unittest.TestCase):
    def test_npm_stalled_body_and_redirects_share_each_attempt_deadline(self):
        attempt_ms = 400
        attempts = 2
        # Only a deadlock watchdog uses wall time, with 10x the total budget.
        watchdog_ms = attempt_ms * attempts * 10
        code = r'''
const assert = require('assert');
const http = require('http');
const {download} = require(process.argv[1]);
const attemptMs = Number(process.argv[2]);
const attempts = Number(process.argv[3]);
const watchdogMs = Number(process.argv[4]);
(async () => {
  const requests = [];
  const timers = [];
  const pending = new Set();
  const requestTimers = [];
  const responseTimers = [];
  const expired = [];
  const clientResources = [];
  const sockets = new Set();
  const socketClosures = [];
  let closedSockets = 0;
  const pendingIds = () => Array.from(pending, timer => timer.id);
  const state = () => JSON.stringify({requests, requestTimers, responseTimers, expired,
    timers: timers.map(({id, delay}) => ({id, delay})), pending: pendingIds(),
    openSockets: sockets.size, closedSockets,
    clientDestroyed: clientResources.map(resource => resource.destroyed)});
  const server = http.createServer((req, res) => {
    requests.push(req.url);
    if (req.url === '/redirect') {
      res.writeHead(302, {location: '/stall'});
      res.write('redirect body never ends');
    } else {res.writeHead(200); res.write('partial');}
  });
  server.on('connection', socket => {
    sockets.add(socket);
    socketClosures.push(new Promise(resolve => socket.once('close', () => {
      sockets.delete(socket);
      closedSockets++;
      resolve();
    })));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const checkDownload = async () => {
    await assert.rejects(download(`http://127.0.0.1:${server.address().port}/redirect`, {
      attemptMs,
      setTimeout(callback, delay) {
        const timer = {id: timers.length + 1, callback, delay};
        timers.push(timer);
        pending.add(timer);
        return timer;
      },
      clearTimeout(timer) {pending.delete(timer);},
      get(url, options, onResponse) {
        requestTimers.push(pendingIds());
        const timer = timers[timers.length - 1];
        const request = http.get(url, options, response => {
          clientResources.push(response);
          responseTimers.push(pendingIds());
          onResponse(response);
          if (response.statusCode === 200) {
            response.once('data', () => {
              // The terminal body has arrived and both server responses stay
              // open. Only this event expires the attempt's original timer;
              // runner load cannot consume the next attempt's budget.
              if (pending.has(timer)) {
                expired.push(timer.id);
                timer.callback();
              }
            });
          }
        });
        clientResources.push(request);
        return request;
      }
    }), /timed out/);
    const attemptIds = Array.from({length: attempts}, (_, index) => index + 1);
    const perHopTimers = attemptIds.flatMap(id => [[id], [id]]);
    assert.deepStrictEqual(requests, attemptIds.flatMap(() => ['/redirect', '/stall']),
      'exactly two attempts, each following one redirect');
    assert.deepStrictEqual(timers.map(timer => timer.delay), attemptIds.map(() => attemptMs),
      'one full deadline must be scheduled per attempt');
    assert.deepStrictEqual(requestTimers, perHopTimers, 'redirect requests share the original deadline');
    assert.deepStrictEqual(responseTimers, perHopTimers, 'redirect and terminal bodies share the original deadline');
    assert.deepStrictEqual(expired, attemptIds, 'each attempt expires only after its terminal body arrives');
    assert.equal(pending.size, 0, 'settled attempts cancel their timers');
    assert(clientResources.every(resource => resource.destroyed), 'client resources destroyed before rejection');
    // Peer close notifications are asynchronous; observe them instead of sleeping.
    await Promise.all(socketClosures);
    assert.equal(sockets.size, 0, 'all redirect and terminal sockets must close');
    assert.equal(closedSockets, attempts * 2, 'each redirect and terminal socket reports closure');
    console.log(state());
  };
  let watchdog;
  try {
    await Promise.race([checkDownload(), new Promise((_, reject) => {
      watchdog = setTimeout(() => reject(new Error(`fixture watchdog exceeded ${watchdogMs} ms`)), watchdogMs);
    })]);
  } catch (error) {
    console.error(`download fixture state: ${state()}`);
    throw error;
  } finally {
    clearTimeout(watchdog);
    for (const socket of sockets) socket.destroy();
    server.close();
  }
})().catch(error => {console.error(error); process.exitCode = 1;});
'''
        result = subprocess.run(["node", "-e", code, str(ROOT / "packaging/npm/install.js"),
                                 str(attempt_ms), str(attempts), str(watchdog_ms)],
                                capture_output=True, text=True, timeout=watchdog_ms / 1000 * 2)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_shell_retry_discards_partial_body_and_fetches_resources_concurrently(self):
        if os.name == "nt":
            self.skipTest("POSIX shell fixture")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            make_archive(root, windows=False, spy=True)
            prefix = root / "prefix"
            env = fake_download_environment(root, prefix, VERSION)
            call = root / "helper-call.json"
            env["INSTALL_CALL"] = str(call)
            curl = root / "commands" / "curl"
            curl.write_text(f'''#!{sys.executable}
import os, pathlib, sys, time
root = pathlib.Path(os.environ['INSTALL_FIXTURE'])
name = sys.argv[-1].rsplit('/', 1)[-1]
assert sys.argv[sys.argv.index('--max-time') + 1] == ('31' if name.endswith('.sha256') else '158')
marker = root / (name + '.started')
marker.touch()
other = name[:-7] if name.endswith('.sha256') else name + '.sha256'
for _ in range(200):
    if (root / (other + '.started')).exists(): break
    time.sleep(.01)
else: sys.exit(91)
retry = root / (name + '.retry')
if not retry.exists():
    retry.touch()
    sys.stdout.write('partial body must be discarded')
    sys.exit(56)
sys.stdout.buffer.write((root / name).read_bytes())
''')
            result = subprocess.run(["sh", str(ROOT / "scripts/install.sh")], env=env, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(call.exists())


class NpmBundleTests(unittest.TestCase):
    def test_zip_and_tar_delegate_complete_source_and_exact_destination(self):
        for windows in ([True] if os.name == "nt" else [False, True]):
            with self.subTest(windows=windows), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                artifact = make_archive(root, windows=windows)
                prefix = root / "vendor with spaces"
                before = old_install(prefix, windows)
                result = self.run_node(artifact, prefix)
                record = json.loads(result.stdout)
                self.assertEqual(record["args"][0], "--install-bundle")
                self.assertEqual(record["args"][2], str(prefix))
                suffix = ".exe" if windows else ""
                self.assertEqual(Path(record["binary"]).name, "haider" + suffix)
                for name in BINARIES:
                    self.assertIn(name + suffix, record["members"])
                self.assertEqual(contents(prefix), before, "wrapper must not publish files itself")
                self.assertFalse(Path(record["args"][1]).exists(), "extraction cleanup follows helper exit")

    def test_missing_payload_never_calls_helper_or_changes_vendor(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact = make_archive(root, windows=True, missing="haider-tui")
            prefix = root / "vendor"
            before = old_install(prefix, True)
            result = self.run_node(artifact, prefix, succeeds=False)
            self.assertIn("Archive did not contain haider-tui.exe", result.stderr)
            self.assertEqual(result.stdout, "")
            self.assertEqual(contents(prefix), before)

    def test_helper_failure_retains_vendor_recovery_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact = make_archive(root, windows=True)
            prefix = root / "vendor"
            before = old_install(prefix, True)
            marker = prefix / ".haider-update-transaction.json"
            marker.write_bytes(b"recovery owned by shared installer")
            before[marker.name] = marker.read_bytes()
            result = self.run_node(artifact, prefix, succeeds=False, fail=True)
            self.assertIn("Bundle installation failed with exit code 74", result.stderr)
            self.assertEqual(contents(prefix), before)

    def run_node(self, artifact, prefix, *, succeeds=True, fail=False):
        code = r'''
const fs = require('fs');
const path = require('path');
const installer = require(process.argv[1]);
const run = (binary, args) => {
  console.log(JSON.stringify({binary, args, members: fs.readdirSync(args[1])}));
  return {status: process.argv[4] === 'fail' ? 74 : 0};
};
installer.installArchive(fs.readFileSync(process.argv[2]), path.basename(process.argv[2]), process.argv[3], run);
'''
        result = subprocess.run(["node", "-e", code, str(ROOT / "packaging/npm/install.js"), str(artifact), str(prefix), "fail" if fail else "ok"], capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode == 0, succeeds, result.stderr)
        return result


@unittest.skipIf(os.name == "nt", "POSIX wrapper is exercised on native Linux/macOS")
class ShellBundleTests(unittest.TestCase):
    def test_verified_archive_delegates_to_staged_helper(self):
        self.run_fixture("success")

    def test_missing_payload_leaves_old_install_exact(self):
        self.run_fixture("missing")

    def test_bad_checksum_leaves_old_install_exact(self):
        self.run_fixture("checksum")

    def test_helper_failure_retains_existing_install(self):
        self.run_fixture("helper")

    def test_explicit_shared_writable_prefix_is_refused_with_actionable_error(self):
        self.run_fixture("unsafe_prefix")

    def test_automatic_split_prefix_falls_back_without_changing_legacy_selection(self):
        script = (ROOT / "scripts/install.sh").read_text()
        # Execute the actual selector with controlled candidates; downloading
        # and publication are covered by the complete-wrapper fixtures below.
        functions = "\n".join(
            name + "() {" + script.split(name + "() {", 1)[1].split("\n}", 1)[0] + "\n}"
            for name in ("fail", "install_dir_eligible", "choose_install_dir")
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            shared = root / "system prefix"
            shared.mkdir()
            fallback = root / "user bin"
            for mode, suffix, expected in ((0o755, "-split", shared), (0o700, "-split", shared),
                                           (0o775, "-split", fallback), (0o775, "", shared)):
                with self.subTest(mode=oct(mode), suffix=suffix):
                    shared.chmod(mode)
                    env = dict(os.environ)
                    env.pop("HAIDER_INSTALL_DIR", None)
                    result = subprocess.run(["sh", "-c", functions + '\nBUNDLE_SUFFIX="$3"\nchoose_install_dir "$1" "$2"',
                                             "prefix-probe", str(shared), str(fallback), suffix], env=env,
                                            capture_output=True, text=True, timeout=30)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), str(expected))
                    self.assertEqual(shared.stat().st_mode & 0o777, mode)

    def run_fixture(self, mode):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact = make_archive(root, windows=False, missing="haider-tui" if mode == "missing" else "", spy=True)
            if mode == "checksum":
                (root / (artifact.name + ".sha256")).write_text("0" * 64 + "  " + artifact.name + "\n")
            prefix = root / "prefix with spaces" / "bin"
            before = old_install(prefix, False)
            if mode == "unsafe_prefix":
                prefix.chmod(0o775)
            marker = prefix / ".haider-update-transaction.json"
            marker.write_bytes(b"recovery owned by shared installer")
            before[marker.name] = marker.read_bytes()
            call = root / "helper-call.json"
            env = fake_download_environment(root, prefix, VERSION)
            env.update(INSTALL_CALL=str(call), INSTALL_EXIT="74" if mode == "helper" else "0")
            result = subprocess.run(["sh", str(ROOT / "scripts/install.sh")], env=env, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode == 0, mode == "success", result.stderr)
            if mode == "unsafe_prefix":
                self.assertIn("choose an owned prefix", result.stderr)
            self.assertEqual(contents(prefix), before, "wrapper delegates all canonical mutation")
            if mode in ("success", "helper"):
                args = json.loads(call.read_text())
                self.assertEqual(args[1], "--install-bundle")
                self.assertEqual(args[3], str(prefix))
                self.assertFalse(Path(args[2]).exists())
            else:
                self.assertFalse(call.exists())


class HistoricalBundleTests(unittest.TestCase):
    def test_historical_pin_installs_original_members_without_executing_legacy_cli(self):
        for requested in ("0.0.969", "v0.0.969"):
            with self.subTest(requested=requested), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                # Deliberately non-executable byte fixtures: historical binaries
                # cannot understand --install-bundle and must never be invoked.
                artifact = make_archive(root, windows=os.name == "nt", version="0.0.969", legacy=True)
                prefix = root / "shared prefix with spaces" / "bin"
                result = self.run_installer(root, prefix, requested)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                extension = ".exe" if os.name == "nt" else ""
                expected = {name + extension: f"new-{name}".encode() for name in ("haider", "haiderd")}
                if platform.system() == "Linux":
                    expected["haider-wayland-portal"] = b"new-haider-wayland-portal"
                self.assertEqual(contents(prefix), expected)
                self.assertNotIn("-split", artifact.name)
                if os.name != "nt":
                    for name in expected:
                        self.assertEqual((prefix / name).stat().st_mode & 0o777, 0o755)

    def test_historical_pin_refuses_missing_member_bad_checksum_and_pending_recovery(self):
        for failure in ("missing", "checksum", "pending"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                artifact = make_archive(root, windows=os.name == "nt", version="0.0.969", legacy=True,
                                        missing="haiderd" if failure == "missing" else "")
                if failure == "checksum":
                    (root / (artifact.name + ".sha256")).write_text("0" * 64 + "  " + artifact.name + "\n")
                prefix = root / "prefix" / "bin"
                before = old_install(prefix, os.name == "nt")
                if failure == "pending":
                    marker = prefix / ".haider-update-transaction.json"
                    marker.write_bytes(b"pending transaction must remain authoritative")
                    before[marker.name] = marker.read_bytes()
                result = self.run_installer(root, prefix, "0.0.969")
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(contents(prefix), before)

    def run_installer(self, root, prefix, version):
        if os.name != "nt":
            return subprocess.run(["sh", str(ROOT / "scripts/install.sh")],
                                  env=fake_download_environment(root, prefix, version),
                                  capture_output=True, text=True, timeout=30)
        wrapper = root / "fixture.ps1"
        wrapper.write_text(r'''
$ErrorActionPreference = 'Stop'
function Invoke-WebRequest {
  param($Uri, $OutFile, $Headers)
  Copy-Item (Join-Path $env:INSTALL_FIXTURE ([IO.Path]::GetFileName($Uri))) $OutFile
}
$SavedPath = [Environment]::GetEnvironmentVariable('Path', 'User')
try {
  [Environment]::SetEnvironmentVariable('Path', ($SavedPath + ';' + $env:HAIDER_INSTALL_DIR), 'User')
  & $env:INSTALL_SCRIPT
} finally {
  [Environment]::SetEnvironmentVariable('Path', $SavedPath, 'User')
}
''')
        env = dict(os.environ, INSTALL_FIXTURE=str(root), INSTALL_SCRIPT=str(ROOT / "scripts/install.ps1"),
                   HAIDER_INSTALL_DIR=str(prefix), HAIDER_VERSION=version, PROCESSOR_ARCHITECTURE="AMD64")
        return subprocess.run(["pwsh", "-NoProfile", "-File", str(wrapper)], env=env,
                              capture_output=True, text=True, timeout=30)


if __name__ == "__main__":
    unittest.main()
