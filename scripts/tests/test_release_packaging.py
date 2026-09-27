#!/usr/bin/env python3
"""Regression tests for the release package substitution and final gates."""

from __future__ import annotations

import contextlib
import importlib.util
import hashlib
import io
import json
import os
import subprocess
import textwrap
import shutil
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts" / "release_packaging.py"
SPEC = importlib.util.spec_from_file_location("release_packaging", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
release_packaging = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_packaging)

VERSION = "9.8.7"
STALE_VERSION = "9.8.6"
SHA256 = "ab" * 32
SHAS = {
    target: f"{index:02x}" * 32
    for index, target in enumerate(
        (*release_packaging.HOMEBREW_TARGETS, release_packaging.WINDOWS_TARGET),
        start=1,
    )
}


def _template(root: Path, newline: bytes) -> Path:
    source = root / "template"
    tools = source / "tools"
    tools.mkdir(parents=True)
    nuspec = newline.join(
        (
            b'<?xml version="1.0" encoding="utf-8"?>',
            b'<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">',
            b"  <metadata>",
            b"    <version>__HAIDER_VERSION__</version>",
            b"    <iconUrl>https://haidercode.ai/logo.svg</iconUrl>",
            b"  </metadata>",
            b"</package>",
            b"",
        )
    )
    install = newline.join(
        (
            b"$version = '__HAIDER_VERSION__'",
            b"$url64 = '__HAIDER_WINDOWS_X64_URL__'",
            b"$checksum64 = '__HAIDER_WINDOWS_X64_SHA256__'",
            b"",
        )
    )
    verification = newline.join(
        (
            b"VERIFICATION",
            b"https://github.com/Rizzist/haider-agent/releases/tag/v__HAIDER_VERSION__",
            b"__HAIDER_WINDOWS_X64_SHA256__",
            b"",
        )
    )
    (source / "haider.nuspec").write_bytes(nuspec)
    (tools / "chocolateyinstall.ps1").write_bytes(install)
    (tools / "VERIFICATION.txt").write_bytes(verification)
    return source


def _nupkg(tree: Path, destination: Path) -> None:
    with zipfile.ZipFile(destination, "w", zipfile.ZIP_DEFLATED) as archive:
        for path in tree.rglob("*"):
            if path.is_file():
                archive.write(path, path.relative_to(tree).as_posix())


def _parse_workflow_structure(source: str) -> dict:
    result: dict = {"jobs": {}, "permissions": {}, "env": {}, "on": {}}
    section = ""
    job: dict | None = None
    step: dict | None = None
    step_section = ""
    for raw in source.splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        line = raw.strip()
        if indent == 0 and line.endswith(":"):
            section = line[:-1]
            continue
        if section == "on" and indent == 2 and line.endswith(":"):
            result["on"][line[:-1]] = {}
        elif section in ("permissions", "env") and indent == 2 and ":" in line:
            key, value = line.split(":", 1)
            result[section][key] = value.strip()
        elif section == "jobs":
            if indent == 2 and line.endswith(":"):
                job = {"steps": []}
                result["jobs"][line[:-1]] = job
                step = None
            elif indent == 4 and job is not None and ":" in line:
                key, value = line.split(":", 1)
                value = value.strip()
                if key != "steps":
                    job[key] = [part.strip() for part in value[1:-1].split(",")] if value.startswith("[") else value
            elif indent == 6 and line.startswith("- ") and job is not None:
                step = {}
                job["steps"].append(step)
                step_section = ""
                key, value = line[2:].split(":", 1)
                step[key] = value.strip()
            elif indent == 8 and step is not None and ":" in line:
                key, value = line.split(":", 1)
                step_section = key if not value.strip() or (key == "run" and value.strip() == "|") else ""
                step[key] = {} if step_section in ("env", "with") else value.strip()
            elif indent >= 10 and step is not None and step_section == "run":
                step["run"] += "\n" + line
            elif indent == 10 and step is not None and step_section in ("env", "with") and ":" in line:
                key, value = line.split(":", 1)
                step[step_section][key] = False if value.strip() == "false" else value.strip()
    return result


class ChocolateyReleaseTests(unittest.TestCase):
    def test_repository_template_has_every_required_placeholder_once(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "rendered"
            release_packaging.render_chocolatey_tree(
                ROOT / "packaging" / "chocolatey", output, VERSION, SHA256
            )

    def test_render_rewrites_every_field_for_lf_and_crlf(self) -> None:
        for name, newline in (("LF", b"\n"), ("CRLF", b"\r\n")):
            with self.subTest(newline=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                source = _template(root, newline)
                output = root / "rendered"
                artifact = root / release_packaging._windows_artifact(VERSION)
                artifact.write_bytes(b"real release artifact")
                artifact_sha = hashlib.sha256(artifact.read_bytes()).hexdigest()
                release_packaging.render_chocolatey_tree(
                    source, output, VERSION, artifact_sha
                )
                package = root / f"haider.{VERSION}.nupkg"
                _nupkg(output, package)
                release_packaging.verify_chocolatey_against_artifact(
                    package, VERSION, artifact
                )

                rendered = b"\n".join(
                    path.read_bytes()
                    for path in (
                        output / "haider.nuspec",
                        output / "tools" / "chocolateyinstall.ps1",
                        output / "tools" / "VERIFICATION.txt",
                    )
                )
                self.assertNotIn(b"__HAIDER_", rendered)
                self.assertIn(VERSION.encode(), rendered)
                self.assertIn(release_packaging._windows_url(VERSION).encode(), rendered)
                self.assertIn(artifact_sha.encode(), rendered)
                self.assertIn(release_packaging._icon_url(VERSION).encode(), rendered)
                self.assertIn(newline, (output / "haider.nuspec").read_bytes())

    def test_render_fails_loudly_when_a_required_token_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = _template(root, b"\r\n")
            install = source / "tools" / "chocolateyinstall.ps1"
            install.write_bytes(
                install.read_bytes().replace(b"__HAIDER_WINDOWS_X64_URL__", b"stale")
            )
            with self.assertRaisesRegex(
                release_packaging.PackagingError,
                r"chocolateyinstall\.ps1.*install URL.*matched 0 times",
            ):
                release_packaging.render_chocolatey_tree(
                    source, root / "rendered", VERSION, SHA256
                )

    def test_post_pack_gate_rejects_a_deliberately_stale_pin(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "rendered"
            artifact = root / release_packaging._windows_artifact(VERSION)
            artifact.write_bytes(b"real release artifact")
            release_packaging.render_chocolatey_from_artifact(
                _template(root, b"\n"), output, VERSION, artifact
            )
            install = output / "tools" / "chocolateyinstall.ps1"
            install.write_text(
                install.read_text().replace(
                    release_packaging._windows_url(VERSION),
                    release_packaging._windows_url(STALE_VERSION),
                ),
                encoding="utf-8",
            )
            package = root / f"haider.{VERSION}.nupkg"
            _nupkg(output, package)
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "install URL mismatch"
            ):
                release_packaging.verify_chocolatey_against_artifact(
                    package, VERSION, artifact
                )


FIX_VERSION = f"{VERSION}.20260927"
VCREDIST = ("vcredist140", "14.51.36231")


class ChocolateyPackageFixTests(unittest.TestCase):
    def render_fix(self, root: Path, newline: bytes = b"\r\n", source: Path | None = None):
        artifact = root / release_packaging._windows_artifact(VERSION)
        artifact.write_bytes(b"unchanged release artifact")
        artifact_sha = hashlib.sha256(artifact.read_bytes()).hexdigest()
        checksum = artifact.with_name(artifact.name + ".sha256")
        checksum.write_text(f"{artifact_sha} *{artifact.name}\n")
        output = root / "rendered"
        release_packaging.render_chocolatey_from_artifact(
            source or _template(root, newline),
            output,
            VERSION,
            artifact,
            package_version=FIX_VERSION,
            dependencies=["=".join(VCREDIST)],
            checksum=checksum,
        )
        return output, artifact, checksum, artifact_sha

    def test_fix_version_keeps_the_original_release_payload_and_adds_dependency(self) -> None:
        for name, newline in (("LF", b"\n"), ("CRLF", b"\r\n")):
            with self.subTest(newline=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                output, artifact, checksum, artifact_sha = self.render_fix(root, newline)
                nuspec = (output / "haider.nuspec").read_bytes()
                install = (output / "tools" / "chocolateyinstall.ps1").read_text()
                verification = (output / "tools" / "VERIFICATION.txt").read_text()

                self.assertIn(f"<version>{FIX_VERSION}</version>".encode(), nuspec)
                self.assertIn(
                    b'<dependency id="vcredist140" version="14.51.36231" />', nuspec
                )
                self.assertNotIn(b"\r\r\n", nuspec)
                if newline == b"\n":
                    self.assertNotIn(b"\r\n", nuspec)
                self.assertIn(f"$version = '{VERSION}'", install)
                self.assertIn(release_packaging._windows_url(VERSION), install)
                self.assertNotIn(FIX_VERSION, install)
                self.assertIn(artifact_sha, install)
                self.assertIn(f"/releases/tag/v{VERSION}\n", verification)
                self.assertNotIn(FIX_VERSION, verification)

                package = root / f"haider.{FIX_VERSION}.nupkg"
                _nupkg(output, package)
                release_packaging.verify_chocolatey_against_artifact(
                    package,
                    VERSION,
                    artifact,
                    package_version=FIX_VERSION,
                    dependencies=[VCREDIST],
                    checksum=checksum,
                )
                # A normal-release verification of the fix package must fail.
                with self.assertRaisesRegex(
                    release_packaging.PackagingError, "nupkg filename mismatch"
                ):
                    release_packaging.verify_chocolatey_against_artifact(
                        package, VERSION, artifact
                    )
                with self.assertRaisesRegex(
                    release_packaging.PackagingError, "nuspec dependencies mismatch"
                ):
                    release_packaging.verify_chocolatey_nupkg(
                        package, VERSION, artifact_sha, package_version=FIX_VERSION
                    )
                with self.assertRaisesRegex(
                    release_packaging.PackagingError, "nuspec dependencies mismatch"
                ):
                    release_packaging.verify_chocolatey_nupkg(
                        package,
                        VERSION,
                        artifact_sha,
                        package_version=FIX_VERSION,
                        dependencies=[("vcredist140", "14.30.30704")],
                    )

    def test_repository_template_renders_a_fix_package(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output, *_ = self.render_fix(
                Path(temporary), source=ROOT / "packaging" / "chocolatey"
            )
            root = release_packaging.ET.fromstring((output / "haider.nuspec").read_bytes())
            dependencies = root.findall(".//{*}dependencies/{*}dependency")
            self.assertEqual(
                [(item.get("id"), item.get("version")) for item in dependencies],
                [VCREDIST],
            )
            self.assertEqual(
                release_packaging._single_xml_text(root, "version", "rendered"),
                FIX_VERSION,
            )

    def test_normal_release_rejects_unexpected_dependencies(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = _template(root, b"\n")
            output = root / "rendered"
            release_packaging.render_chocolatey_tree(
                source, output, VERSION, SHA256, dependencies=["=".join(VCREDIST)]
            )
            package = root / f"haider.{VERSION}.nupkg"
            _nupkg(output, package)
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "nuspec dependencies mismatch"
            ):
                release_packaging.verify_chocolatey_nupkg(package, VERSION, SHA256)

    def test_invalid_fix_versions_dependencies_and_checksums_fail(self) -> None:
        for package_version in (
            "9.8.6.20260927",
            "9.8.7-20260927",
            "9.8.7.2026.0927",
            "9.8.70",
        ):
            with self.subTest(package_version=package_version), self.assertRaisesRegex(
                release_packaging.PackagingError, "invalid package-fix version"
            ):
                release_packaging._package_version(VERSION, package_version)
        for dependency in ("vcredist140", "vcredist140=latest", "bad id=1.0", "x=[1.0,)"):
            with self.subTest(dependency=dependency), self.assertRaises(
                release_packaging.PackagingError
            ):
                release_packaging._dependencies([dependency])
        with self.assertRaisesRegex(release_packaging.PackagingError, "duplicate"):
            release_packaging._dependencies(["a=1.0", "A=2.0"])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _, artifact, checksum, _ = self.render_fix(root)
            checksum.write_text(f"{'00' * 32} *{artifact.name}\n")
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "does not match"
            ):
                release_packaging.published_windows_sha256(artifact, VERSION, checksum)

    def test_cli_renders_and_verifies_a_fix_package(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _, artifact, checksum, _ = self.render_fix(root)
            output = root / "cli"
            common = [
                "--version", VERSION,
                "--artifact", str(artifact),
                "--package-version", FIX_VERSION,
                "--dependency", "=".join(VCREDIST),
                "--checksum", str(checksum),
            ]
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(
                    release_packaging.main(
                        ["render-chocolatey", "--source", str(_template(root / "t", b"\n")),
                         "--output", str(output), *common]
                    ),
                    0,
                )
                package = root / f"haider.{FIX_VERSION}.nupkg"
                _nupkg(output, package)
                self.assertEqual(
                    release_packaging.main(
                        ["verify-chocolatey", "--nupkg", str(package), *common]
                    ),
                    0,
                )
            # Omitting the dependency must fail the post-pack gate.
            without_dependency = [*common[:6], "--checksum", str(checksum)]
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(
                    release_packaging.main(
                        ["verify-chocolatey", "--nupkg", str(package), *without_dependency]
                    ),
                    1,
                )

    def test_package_fix_workflow_scopes_secret_and_gates_publish(self) -> None:
        workflow_text = (ROOT / ".github/workflows/chocolatey-package-fix.yml").read_text()
        try:
            import yaml
        except ImportError:
            # CI's stock Python has no YAML package. Parse the relevant mappings
            # with strict indentation instead of silently skipping this gate.
            workflow = _parse_workflow_structure(workflow_text)
        else:
            workflow = yaml.safe_load(workflow_text)
        self.assertEqual(workflow_text.count("secrets.CHOCO_API_KEY"), 1)
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        triggers = workflow.get("on", workflow.get(True))
        self.assertIn("pull_request", triggers)
        self.assertIn("workflow_dispatch", triggers)
        jobs = workflow["jobs"]
        self.assertEqual(jobs["accept"]["needs"], "pack")
        self.assertEqual(jobs["publish"]["needs"], ["pack", "accept"])
        self.assertEqual(jobs["publish"]["if"], "github.event_name == 'workflow_dispatch'")
        self.assertNotIn("CHOCO_API_KEY", workflow["env"])
        push_steps = [step for step in jobs["publish"]["steps"]
                      if "-Mode Push" in step.get("run", "")]
        self.assertEqual(len(push_steps), 1)
        self.assertEqual(push_steps[0]["env"]["CHOCO_API_KEY"], "${{ secrets.CHOCO_API_KEY }}")
        for job in jobs.values():
            self.assertNotIn("CHOCO_API_KEY", job.get("env", {}))
            for step in job["steps"]:
                if step is not push_steps[0]:
                    self.assertNotIn("CHOCO_API_KEY", step.get("env", {}))
                if step.get("uses", "").startswith("actions/checkout@"):
                    self.assertIs(step["with"]["persist-credentials"], False)
        self.assertIn("choco push", (ROOT / "scripts/chocolatey_publish.ps1").read_text())
        self.assertNotIn("gh release upload", workflow_text)

    @unittest.skipUnless(shutil.which("pwsh"), "PowerShell is required for publish behavior")
    def test_chocolatey_publish_behavior(self) -> None:
        script = ROOT / "scripts/chocolatey_publish.ps1"
        harness = r"""
function Invoke-WebRequest {
  $index = $global:queryIndex
  $global:queryIndex++
  $statuses = $env:MOCK_STATUSES.Split(',')
  $status = $statuses[[Math]::Min($index, $statuses.Length - 1)]
  Add-Content $env:MOCK_TRACE "query:$status"
  if ($status -eq 'network') { throw 'Synthetic network failure' }
  if ($status -in @('exact', 'wrong', 'prefix', 'malformed')) {
    $version = $env:PACKAGE_VERSION
    if ($status -eq 'wrong') { $version = '0.0.972' }
    if ($status -eq 'prefix') { $version += '0' }
    $uri = "https://community.chocolatey.org/api/v2/Packages(Id='haider',Version='$version')"
    $content = "<entry xmlns='http://www.w3.org/2005/Atom' xmlns:d='http://schemas.microsoft.com/ado/2007/08/dataservices' xmlns:m='http://schemas.microsoft.com/ado/2007/08/dataservices/metadata'><id>$uri</id><title>haider</title><m:properties><d:Version>$version</d:Version></m:properties></entry>"
    if ($status -eq 'malformed') { $content = '<entry><' }
    return [pscustomobject]@{ StatusCode = 200; Content = $content }
  }
  return [pscustomobject]@{ StatusCode = [int]$status; Content = '' }
}
function choco {
  Add-Content $env:MOCK_TRACE 'push'
  $global:LASTEXITCODE = [int]$env:MOCK_PUSH_EXIT
  Write-Output $env:MOCK_PUSH_OUTPUT
}
$global:queryIndex = 0
try {
  if ($env:MOCK_CHECK -eq '1') {
    & $env:PUBLISH_SCRIPT -Mode Check
    $env:PACKAGE_PRESENT = (Get-Content $env:GITHUB_OUTPUT | Select-Object -Last 1).Split('=')[1]
  }
  if ($env:MOCK_DO_PUSH -eq '1') { & $env:PUBLISH_SCRIPT -Mode Push }
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} catch {
  Write-Output "EXPECTED_ERROR: $_"
  exit 1
}
"""
        # name, responses, key, check, push exit, expected exit, push calls,
        # minimum post-push queries, summary fragment, setting override
        cases = [
            ("missing key", "404", "", False, 0, 1, 0, 0, "NOT pushed", None),
            ("check incomplete", "404", "fake", False, 0, 1, 0, 0, "", None),
            ("present before push", "exact", "fake", True, 0, 0, 0, 0, "push skipped", None),
            ("push confirmed", "404,exact", "fake", True, 0, 0, 1, 1, "pushed and confirmed", None),
            ("push absent", "404", "fake", True, 0, 1, 1, 2, "NOT confirmed", None),
            ("push 503", "404,503", "fake", True, 0, 1, 1, 2, "NOT confirmed", None),
            ("push 501", "404,501", "fake", True, 0, 1, 1, 2, "NOT confirmed", None),
            ("push network", "404,network", "fake", True, 0, 1, 1, 2, "NOT confirmed", None),
            ("push wrong version", "404,wrong", "fake", True, 0, 1, 1, 1, "NOT confirmed", None),
            ("push prefix version", "404,prefix", "fake", True, 0, 1, 1, 1, "NOT confirmed", None),
            ("push malformed XML", "404,malformed", "fake", True, 0, 1, 1, 1, "NOT confirmed", None),
            ("push nontransient 403", "404,403", "fake", True, 0, 1, 1, 1, "NOT confirmed", None),
            ("delayed indexing", "404,404,404,exact", "fake", True, 0, 0, 1, 3, "pushed and confirmed", None),
            ("push 401", "404", "fake", True, 19, 19, 1, 2, "NOT pushed", None),
            ("push 403", "404", "fake", True, 23, 23, 1, 2, "NOT pushed", None),
            ("409 now present", "404,exact", "fake", True, 19, 0, 1, 1, "idempotent success", None),
            ("409 delayed present", "404,404,exact", "fake", True, 19, 0, 1, 2, "idempotent success", None),
            ("409 still absent", "404", "fake", True, 19, 19, 1, 2, "NOT pushed", None),
            ("push 500", "404", "fake", True, 42, 42, 1, 2, "NOT pushed", None),
            ("post-push feed error", "404,503", "fake", True, 19, 19, 1, 2, "NOT pushed", None),
            ("feed query error", "503", "fake", True, 0, 1, 0, 0, "", None),
            ("invalid timeout", "404", "fake", True, 0, 1, 0, 0, "", "0"),
            ("nonnumeric timeout", "404", "fake", True, 0, 1, 0, 0, "", "ten"),
            ("too large timeout", "404", "fake", True, 0, 1, 0, 0, "", "901"),
        ]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            harness_path = root / "publish-harness.ps1"
            harness_path.write_text(textwrap.dedent(harness))
            for name, statuses, key, check, push_exit, expected, pushes, post_queries, summary_fragment, setting in cases:
                with self.subTest(name=name):
                    trace = root / "trace.txt"
                    summary = root / "summary.txt"
                    output = root / "output.txt"
                    for path in (trace, summary, output):
                        path.write_text("")
                    env = {**os.environ, "PUBLISH_SCRIPT": str(script),
                           "PACKAGE_VERSION": FIX_VERSION, "CHOCO_API_KEY": key,
                           "PACKAGE_PRESENT": "unknown" if name == "check incomplete" else "false", "MOCK_STATUSES": statuses,
                           "MOCK_CHECK": "1" if check else "0", "MOCK_DO_PUSH": "0" if name == "feed query error" else "1",
                           "MOCK_PUSH_EXIT": str(push_exit), "MOCK_PUSH_OUTPUT": "synthetic push",
                           "MOCK_TRACE": str(trace), "GITHUB_OUTPUT": str(output),
                           "GITHUB_STEP_SUMMARY": str(summary),
                           "CHOCO_CONFIRM_TIMEOUT_SECONDS": setting or "1",
                           "CHOCO_CONFIRM_POLL_INTERVAL_MS": "100"}
                    result = subprocess.run(["pwsh", "-NoProfile", "-File", str(harness_path)],
                                            env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                    events = trace.read_text().splitlines()
                    self.assertEqual(events.count("push"), pushes)
                    if pushes:
                        self.assertGreaterEqual(len(events[events.index("push") + 1:]), post_queries)
                    if summary_fragment:
                        self.assertIn(summary_fragment, summary.read_text())


class SiblingPackagerTests(unittest.TestCase):
    def test_homebrew_and_scoop_repin_and_gate_all_pins(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            packaging = Path(temporary) / "packaging"
            (packaging / "homebrew").mkdir(parents=True)
            (packaging / "scoop").mkdir(parents=True)
            shutil.copy2(
                ROOT / "packaging" / "homebrew" / "haider.rb",
                packaging / "homebrew" / "haider.rb",
            )
            shutil.copy2(
                ROOT / "packaging" / "scoop" / "haider.json",
                packaging / "scoop" / "haider.json",
            )
            release_packaging.repin_homebrew_scoop(packaging, VERSION, SHAS)
            release_packaging.verify_homebrew_scoop(packaging, VERSION, SHAS)

            scoop_path = packaging / "scoop" / "haider.json"
            scoop = json.loads(scoop_path.read_text())
            scoop["architecture"]["64bit"]["url"] = release_packaging._windows_url(
                STALE_VERSION
            )
            scoop_path.write_text(json.dumps(scoop), encoding="utf-8")
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "Scoop URL mismatch"
            ):
                release_packaging.verify_homebrew_scoop(packaging, VERSION, SHAS)

    def test_npm_archive_gate_uses_the_packed_version_and_dynamic_urls(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            package = Path(temporary) / "haider-agent.tgz"
            metadata = json.loads(
                (ROOT / "packaging" / "npm" / "package.json").read_text()
            )
            metadata["version"] = VERSION
            installer = (ROOT / "packaging" / "npm" / "install.js").read_bytes()
            with tarfile.open(package, "w:gz") as archive:
                for name, data in (
                    ("package/package.json", json.dumps(metadata).encode()),
                    ("package/install.js", installer),
                ):
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))
            release_packaging.verify_npm_archive(package, VERSION)
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "npm version mismatch"
            ):
                release_packaging.verify_npm_archive(package, STALE_VERSION)


class SplitArchiveTests(unittest.TestCase):
    def archive(self, root, *, windows, missing="", bad_checksum=False, legacy=False):
        target = release_packaging.WINDOWS_TARGET if windows else "aarch64-apple-darwin"
        top = f"haider-v{VERSION}-{target}" + ("" if legacy else "-split")
        artifact = root / (top + (".zip" if windows else ".tar.xz"))
        names = [name for name in ("haider", "haider-tui", "haiderd") if name != missing]
        if windows:
            with zipfile.ZipFile(artifact, "w") as archive:
                archive.writestr(top + "/", b"")
                for name in names:
                    archive.writestr(f"{top}/{name}.exe", b"binary")
                archive.writestr(f"{top}/haider.cmd", b"launcher")
                archive.writestr(f"{top}/README.txt", b"readme")
        else:
            with tarfile.open(artifact, "w:xz") as archive:
                info = tarfile.TarInfo(top)
                info.type = tarfile.DIRTYPE
                archive.addfile(info)
                for name in names:
                    info = tarfile.TarInfo(f"{top}/{name}")
                    info.mode = 0o755
                    info.size = 6
                    archive.addfile(info, io.BytesIO(b"binary"))
        digest = "00" * 32 if bad_checksum else hashlib.sha256(artifact.read_bytes()).hexdigest()
        artifact.with_name(artifact.name + ".sha256").write_text(f"{digest}  {artifact.name}\n")
        return artifact, target

    def test_actual_tar_and_zip_payloads_require_all_siblings_and_checksum(self):
        for windows in (False, True):
            for missing, bad_checksum in (("", False), ("haider-tui", False), ("", True)):
                with self.subTest(windows=windows, missing=missing, bad_checksum=bad_checksum), tempfile.TemporaryDirectory() as temporary:
                    artifact, target = self.archive(Path(temporary), windows=windows, missing=missing, bad_checksum=bad_checksum)
                    if missing or bad_checksum:
                        with self.assertRaises(release_packaging.PackagingError):
                            release_packaging.verify_split_bundle(artifact, target)
                    else:
                        release_packaging.verify_split_bundle(artifact, target)

    def test_legacy_canonical_mac_archive_accepts_only_original_pair(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact, target = self.archive(root, windows=False, missing="haider-tui", legacy=True)
            release_packaging.verify_legacy_bundle(artifact, target)
            with self.assertRaises(release_packaging.PackagingError):
                release_packaging.verify_legacy_bundle(artifact, release_packaging.WINDOWS_TARGET)
            artifact, target = self.archive(root, windows=False, legacy=True)
            with self.assertRaisesRegex(release_packaging.PackagingError, "unexpected or empty"):
                release_packaging.verify_legacy_bundle(artifact, target)

    def test_release_and_manager_templates_preserve_split_format_and_public_entrypoint(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        for fragment in ("-p haider-tui-exe", 'cp "$BIN" "$TBIN" "$DBIN"', "haider-tui --version", "verify-bundle"):
            if fragment == "haider-tui --version":
                self.assertIn('"$TBIN" --version', workflow)
            else:
                self.assertIn(fragment, workflow)
        formula = (ROOT / "packaging/homebrew/haider.rb").read_text()
        self.assertIn('"#{source}/haider-tui"', formula)
        scoop = json.loads((ROOT / "packaging/scoop/haider.json").read_text())
        self.assertEqual(scoop["bin"], ["haider.exe"])
        self.assertIn("haider-tui.exe", "\n".join(scoop["pre_install"]))
        npm = json.loads((ROOT / "packaging/npm/package.json").read_text())
        self.assertEqual(npm["bin"], {"haider": "bin/haider.js"})
        choco = (ROOT / "packaging/chocolatey/tools/chocolateyinstall.ps1").read_text()
        self.assertIn("haider-tui.exe", choco)
        self.assertIn("$binary.ignore", choco)


if __name__ == "__main__":
    unittest.main()
