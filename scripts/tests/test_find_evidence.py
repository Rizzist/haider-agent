"""Exercise the evidence CLI through a strict gh API stub (no credentials)."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "release/find-evidence.py"
SHA = "0123456789abcdef0123456789abcdef01234567"


def run_row(**changes):
    row = dict(id=12, head_sha=SHA, head_branch="wave-970", event="push",
               status="completed", conclusion="success")
    return dict(row, **changes)


class EvidenceTests(unittest.TestCase):
    def lookup(self, pages, *args, api_exit=0):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stub = root / "gh"
            stub.write_text(
                f'#!{sys.executable}\nimport json, os, sys\n'
                'from pathlib import Path\n'
                'Path(os.environ["CALLS"]).write_text(json.dumps(sys.argv[1:]))\n'
                'print(os.environ["RESPONSE"])\n'
                'sys.exit(int(os.environ["API_EXIT"]))\n', encoding="utf-8")
            stub.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       GITHUB_REPOSITORY="owner/repo", GITHUB_RUN_ID="99",
                       GITHUB_OUTPUT=str(root / "output"), CALLS=str(root / "calls"),
                       RESPONSE=json.dumps(pages), API_EXIT=str(api_exit))
            result = subprocess.run([sys.executable, str(SCRIPT), "ship-gate.yml", SHA, *args],
                                    env=env, capture_output=True, text=True)
            output = (root / "output").read_text() if (root / "output").exists() else ""
            calls = json.loads((root / "calls").read_text()) if (root / "calls").exists() else []
            return result, output, calls

    def test_any_ref_event_and_later_page_success(self):
        for branch, event in [("wave-970", "push"), ("main", "push"), ("candidate", "workflow_dispatch")]:
            with self.subTest(branch=branch, event=event):
                result, output, calls = self.lookup([
                    {"workflow_runs": [run_row(id=15, conclusion="failure")]},
                    {"workflow_runs": [run_row(head_branch=branch, event=event)]},
                ])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output, "satisfied=true\nrun_id=12\n")
                self.assertEqual(calls, ["api", "--method", "GET", "--paginate", "--slurp",
                    f"repos/owner/repo/actions/workflows/ship-gate.yml/runs?head_sha={SHA}&per_page=100"])

    def test_misses_never_satisfy(self):
        cases = [[], [run_row(head_sha="f" * 40)], [run_row(id=99)]]
        cases += [[run_row(status=status)] for status in ["queued", "in_progress", "waiting", "pending"]]
        cases += [[run_row(conclusion=c)] for c in [None, "failure", "cancelled", "skipped", "neutral", "timed_out"]]
        for rows in cases:
            with self.subTest(rows=rows):
                result, output, _ = self.lookup([{"workflow_runs": rows}])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output, "satisfied=false\nrun_id=\n")

    def test_newest_success_and_explicit_exclusion(self):
        result, output, _ = self.lookup([{"workflow_runs": [run_row(id=10), run_row(id=20)]}],
                                        "--exclude-run-id", "20")
        self.assertEqual(result.returncode, 0)
        self.assertIn("run_id=10\n", output)

    def test_malformed_and_api_errors_fail_without_outputs(self):
        for pages in [{"message": "forbidden"}, [], [{}], [{"workflow_runs": [None]}],
                      [{"workflow_runs": [run_row(id="12")]}]]:
            result, output, _ = self.lookup(pages)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(output, "")
        result, output, _ = self.lookup([{"workflow_runs": []}], api_exit=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, "")


if __name__ == "__main__":
    unittest.main()

# API contract tests for reusable-call artifacts: an artifact proves bytes,
# never a completed ship-gate verdict.
import importlib.util
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('find_evidence', SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ArtifactLookupTests(unittest.TestCase):
    def test_present_missing_expired_and_wrong_name(self):
        for artifact, expected in [({'name': 'build', 'expired': False}, True),
                                   ({'name': 'build', 'expired': True}, False),
                                   ({'name': 'other', 'expired': False}, False),
                                   ({'name': 'build'}, False)]:
            with patch.object(MODULE, 'api', side_effect=[
                [{'id': 123, 'head_sha': SHA, 'status': 'in_progress'}],
                [{'artifacts': []}, {'artifacts': [artifact]}],
            ]):
                self.assertEqual(MODULE.artifact_exists('owner/repo', '123', 'build', SHA), expected)

    def test_wrong_sha_or_run_and_api_failure_rejected(self):
        for response in [[{'id': 123, 'head_sha': 'f' * 40}], [{'id': 456, 'head_sha': SHA}], [{}]]:
            with patch.object(MODULE, 'api', return_value=response):
                with self.assertRaises(ValueError):
                    MODULE.artifact_exists('owner/repo', '123', 'build', SHA)
        with patch.object(MODULE, 'api', side_effect=subprocess.CalledProcessError(1, 'gh')):
            with self.assertRaises(subprocess.CalledProcessError):
                MODULE.artifact_exists('owner/repo', '123', 'build', SHA)
