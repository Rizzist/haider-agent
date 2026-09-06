from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest import mock

import thinexe_legacy_upgrade as legacy


class HistoricalUpgradeFixtureTests(unittest.TestCase):
    def test_archive_keeps_the_exact_historical_member_surface(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            sources = {}
            for name in legacy.NAMES:
                path = root / name
                path.write_bytes(name.encode())
                sources[name] = path
            for negative in (False, True):
                selected = sources if negative else {key: sources[key] for key in ("haider", "haiderd")}
                path = root / f"archive-{negative}.xz"
                legacy.make_archive(path, "haider-v0.0.970-fixture", selected)
                with tarfile.open(path, "r:xz") as archive:
                    self.assertEqual(archive.getnames(), ["haider-v0.0.970-fixture"] +
                                     ["haider-v0.0.970-fixture/" + name for name in selected])
                    self.assertTrue(all(member.isfile() and member.mode & 0o100
                                        for member in archive.getmembers()[1:]))

    def test_environment_is_child_only_and_removes_credential_proxy_inheritance(self):
        with tempfile.TemporaryDirectory() as scratch, mock.patch.dict(os.environ, {
            "HTTPS_PROXY": "https://unrelated", "NO_PROXY": "github.com",
            "GITHUB_TOKEN": "do-not-forward", "HAIDER_GITHUB_TOKEN": "do-not-forward",
            "HAIDER_PROFILE_DIR": "/never-use", "SSL_CERT_FILE": "/unrelated",
        }):
            original = dict(os.environ)
            env = legacy.child_environment(Path(scratch), "http://127.0.0.1:1234", Path("/fixture.pem"))
            self.assertEqual(dict(os.environ), original)
            self.assertNotIn("GITHUB_TOKEN", env)
            self.assertNotIn("HAIDER_GITHUB_TOKEN", env)
            self.assertEqual(env["NO_PROXY"], "")
            self.assertEqual(env["HTTPS_PROXY"], "http://127.0.0.1:1234")
            self.assertEqual(env["CURL_CA_BUNDLE"], "/fixture.pem")
            self.assertEqual(int(env["HAIDER_RUN_DAEMON_IDLE_TTL_MS"]), legacy.LIVE_FIXTURE_IDLE_TTL_MS)
            self.assertGreater(legacy.LIVE_FIXTURE_IDLE_TTL_MS, legacy.UPDATE_TIMEOUT * 1000)
            self.assertLessEqual(legacy.LIVE_FIXTURE_IDLE_TTL_MS, 3_600_000)

    def test_system_curl_accepts_local_ca_and_connect_proxy_without_host_trust_change(self):
        if not Path(legacy.CURL).exists() or not Path("/usr/bin/openssl").exists():
            self.skipTest("native historical fixture requires /usr/bin/curl and openssl")
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            archive = root / "fixture.tar.xz"
            archive.write_bytes(b"fixture archive")
            cert, key = legacy.create_certificate(root)
            checked = []
            fixture = legacy.Fixture("0.0.970", "aarch64-apple-darwin", "owner/repo", archive,
                                     lambda: checked.append(True))
            with legacy.proxy(fixture, cert, key) as address:
                env = legacy.child_environment(root / "child", address, cert)
                release_url = "https://api.github.com/repos/owner/repo/releases?per_page=100&page=1"
                result = subprocess.run([legacy.CURL, "--fail", "--silent", "--show-error",
                                         "--max-time", "15", release_url], env=env,
                                        capture_output=True, timeout=legacy.VERSION_QUERY.seconds)
                self.assertEqual(result.returncode, 0, result.stderr)
                release = json.loads(result.stdout)[0]
                self.assertEqual(release["tag_name"], "v0.0.970")
                for asset in release["assets"]:
                    result = subprocess.run([legacy.CURL, "--fail", "--silent", "--show-error",
                                             "--max-time", "15", asset["browser_download_url"]],
                                            env=env, capture_output=True, timeout=legacy.VERSION_QUERY.seconds)
                    self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(checked, [True, True])
                self.assertFalse(fixture.errors)
                self.assertEqual(len(fixture.requests), 3)

    def test_unexpected_network_destination_is_never_forwarded(self):
        with tempfile.TemporaryDirectory() as scratch:
            archive = Path(scratch) / "fixture"
            archive.write_bytes(b"fixture")
            fixture = legacy.Fixture("0.0.970", "test", "owner/repo", archive)
            with self.assertRaisesRegex(legacy.ProofError, "unexpected fixture request"):
                fixture.response("unrelated.example", "/")

    def test_transaction_cleanup_does_not_hide_backups_or_partial_markers(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            (root / ".haider-update.lock").touch()
            legacy.transaction_clean(root)
            for name in (".haider-tui-old-1", ".haider-update.transaction.json", ".haider-update-stage-1"):
                path = root / name
                path.touch()
                with self.assertRaisesRegex(legacy.ProofError, "not cleaned"):
                    legacy.transaction_clean(root)
                path.unlink()

    def test_positive_and_historical_failures_do_not_suppress_independent_negative(self):
        report = {}
        visited = []
        def exercise(label, mode, row):
            visited.append((label, mode))
            row["commands"] = [{"stdout": "raw evidence", "returncode": 70}]
            if label != "negative":
                raise legacy.ProofError(label + " failure remains a failure")
            row["passed"] = True
        legacy.run_independent_cases(report, exercise)
        self.assertEqual(visited, [("positive", "persistent"), ("historical_autospawn", "autospawn"),
                                   ("negative", "none")])
        self.assertEqual(report["negative"]["status"], "PASS")
        self.assertEqual(report["positive"]["status"], "FAIL")
        self.assertEqual(report["historical_autospawn"]["status"], "FAIL")
        self.assertFalse(report["passed"])
        self.assertEqual(report["status"], "FAIL")
        self.assertEqual(len(report["errors"]), 2)
        self.assertEqual(report["historical_autospawn"]["commands"][0]["stdout"], "raw evidence")

    def test_independent_case_success_requires_all_existing_assertions_to_finish(self):
        report = {}
        legacy.run_independent_cases(report, lambda _label, _mode, _row: None)
        self.assertFalse(report["passed"])
        self.assertEqual(len(report["errors"]), 3)
        report = {}
        legacy.run_independent_cases(report, lambda _label, _mode, row: row.update(passed=True))
        self.assertTrue(report["passed"])

    def test_migration_signature_reference_preserves_valid_bytes_while_legacy_forces_signing(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            source = root / "source"
            source.write_bytes(b"valid release signature bytes")
            with mock.patch.object(legacy.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
                migration = legacy.signed_reference(source, root / "preserved", historical=False)
                self.assertEqual(migration["sha256"], legacy.sha256_file(source))
                self.assertEqual(run.call_args.args[0][1:3], ["--verify", "--strict"])
                self.assertEqual(run.call_count, 1)
            def resign(argv, **_kwargs):
                Path(argv[-1]).write_bytes(b"historical ad-hoc signature bytes")
                return subprocess.CompletedProcess(argv, 0)
            with mock.patch.object(legacy.subprocess, "run", side_effect=resign) as run:
                historical = legacy.signed_reference(source, root / "normalized")
                self.assertIn("--force", run.call_args.args[0])
                self.assertNotEqual(historical["sha256"], migration["sha256"])

    def test_signature_references_use_canonical_basenames_in_separate_phase_directories(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            sources = {name: root / "inputs" / name for name in (*legacy.NAMES, "compat")}
            calls = []
            def reference(source, destination, *, historical=True):
                calls.append((source, destination, historical))
                return {"sha256": str(destination)}
            with mock.patch.object(legacy, "signed_reference", side_effect=reference):
                result = legacy.signature_references(sources, root / "references")
            self.assertEqual(calls, [
                (sources["compat"], root / "references/historical/haider", True),
                (sources["haiderd"], root / "references/historical/haiderd", True),
                (sources["haider"], root / "references/migration/haider", False),
                (sources["haider-tui"], root / "references/migration/haider-tui", False),
            ])
            self.assertNotEqual(result["historical"]["compat"], result["migration"]["haider"])
            self.assertEqual(result["migration"]["haiderd"], result["historical"]["haiderd"])

    def test_failure_diagnostics_preserve_marker_phase_members_and_daemon_log_tail(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            install = root / "bin"
            install.mkdir()
            (install / "haider").write_bytes(b"new canonical compatibility executable")
            marker = install / ".haider-update-transaction.json"
            raw = b'{"phase":"drain_signaled","schema":"haider.update.transaction.v1"}'
            marker.write_bytes(raw)
            (install / ".haider-old-1").write_bytes(b"old executable")
            profile = root / "profile"
            profile.mkdir()
            (profile / "daemon.log").write_bytes(b"x" * (70 * 1024) + b"forced after launcher exit")
            facts = legacy.diagnostics(root, install)
            self.assertEqual(facts["transaction_phase"], "drain_signaled")
            self.assertEqual(facts["marker"]["raw"].encode(), raw)
            self.assertIn(".haider-old-1", facts["transaction_assets"])
            self.assertEqual(facts["members"]["haider"]["sha256"], legacy.sha256_file(install / "haider"))
            self.assertEqual(len(facts["logs"][0]["tail"]), 64 * 1024)
            self.assertTrue(facts["logs"][0]["tail"].endswith("forced after launcher exit"))
            self.assertEqual(marker.read_bytes(), raw)

    def test_failed_scratch_is_retained_even_when_keep_scratch_is_false(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch) / "case"
            root.mkdir()
            marker = root / "marker"
            marker.write_bytes(b"raw recovery evidence")
            report = {"passed": False, "scratch": str(root)}
            legacy.finish_scratch(root, False, report)
            self.assertEqual(marker.read_bytes(), b"raw recovery evidence")
            self.assertTrue(report["scratch_retained_for_failure"])
            report = {"passed": True, "scratch": str(root)}
            legacy.finish_scratch(root, False, report)
            self.assertFalse(root.exists())
            self.assertNotIn("scratch", report)


if __name__ == "__main__":
    unittest.main()
