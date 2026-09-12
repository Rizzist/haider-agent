"""Run the real Bash driver with a recording Cargo transport; never build Rust."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/ci-test.sh'
EXPECTED = {'haider-platform', 'haider-protocol', 'haider-accounts', 'haider-core',
            'haider-pdf', 'haider-provider', 'haider-daemon', 'haider-daemond',
            'haider-rpc', 'haider-tui', 'haider-tui-exe', 'haider-cli', 'haider-compat',
            'haider-store', 'haider-tools', 'haider-client', 'haider-verify', 'haider-webextract',
            'haider-stt', 'xtask'}


@unittest.skipUnless(
    os.name == 'posix',
    'POSIX Bash driver and executable shims; covered by xplat-check Linux check '
    "leg's pipeline regression tests (Windows Python may resolve bash to WSL)")
class ShardTests(unittest.TestCase):
    def command(self, *args, **overrides):
        env = dict(os.environ)
        env.pop('HAIDER_CI_SHARD_INDEX', None)
        env.pop('HAIDER_CI_SHARD_TOTAL', None)
        env.update(overrides)
        return subprocess.run(['bash', SCRIPT.as_posix(), *args], cwd=ROOT,
                              env=env, capture_output=True, text=True)

    def test_three_disjoint_shards_cover_every_crate_once(self):
        baseline = self.command('--list-crates')
        self.assertEqual(baseline.returncode, 0, baseline.stderr)
        self.assertEqual(set(baseline.stdout.split()), EXPECTED)
        selected = []
        for index in range(1, 4):
            result = self.command('--list-crates', HAIDER_CI_SHARD_INDEX=str(index), HAIDER_CI_SHARD_TOTAL='3')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.split(), sorted(EXPECTED)[index - 1::3])
            selected.extend(result.stdout.split())
        self.assertEqual(len(selected), len(EXPECTED))
        self.assertEqual(set(selected), EXPECTED)
        # The named Windows clipboard gate is in shard 1, alongside its crate.
        self.assertIn('haider-tui', sorted(EXPECTED)[::3])

    def test_invalid_or_empty_shards_fail_before_cargo(self):
        for index, count in [('0','3'),('4','3'),('1','0'),('a','3'),('2','1'),('20','20')]:
            result = self.command('--list-crates', HAIDER_CI_SHARD_INDEX=index, HAIDER_CI_SHARD_TOTAL=count)
            self.assertEqual(result.returncode, 2, result.stdout + result.stderr)

    def driver(self, index=None, fail_crate=''):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / 'bin'
            bin_dir.mkdir()
            cargo = bin_dir / 'cargo'
            cargo.write_text('''#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$CARGO_CALLS"
if [[ "$*" == *"--no-fail-fast -p $FAIL_CRATE "* && -n "$FAIL_CRATE" ]]; then
  echo 'test simulated_failure ... FAILED'
  exit 1
fi
''', newline='\n')
            cargo.chmod(0o755)
            timeout = bin_dir / 'timeout'
            timeout.write_text('#!/usr/bin/env bash\nshift\nexec "$@"\n', newline='\n')
            timeout.chmod(0o755)
            kwargs = dict(PATH=str(bin_dir) + os.pathsep + os.environ['PATH'],
                          CARGO_CALLS=(root / 'calls').as_posix(), FAIL_CRATE=fail_crate,
                          HAIDER_CI_TEST_LOG_DIR=(root / 'logs').as_posix(),
                          GITHUB_STEP_SUMMARY=(root / 'summary').as_posix(), RUNNER_OS='Windows')
            if index is not None:
                kwargs.update(HAIDER_CI_SHARD_INDEX=str(index), HAIDER_CI_SHARD_TOTAL='3')
            result = self.command(**kwargs)
            self.assertTrue((root / 'calls').exists(), result.stdout + result.stderr)
            calls = (root / 'calls').read_text().splitlines()
            summary = (root / 'logs/failure-summary.md').read_text()
            return result, calls, summary

    def test_compile_and_execute_only_selected_crates_with_siblings_and_streamed_test(self):
        executions = []
        streamed = 0
        for index in range(1, 4):
            result, calls, _ = self.driver(index)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(calls[0], 'build --locked -p haider-cli -p haider-daemond -p haider-tui-exe -p haider-tools --bins')
            selected = sorted(EXPECTED)[index - 1::3]
            self.assertEqual(calls[1], 'test ' + ' '.join('-p ' + c for c in selected) + ' --no-run --locked')
            for call in calls[2:]:
                self.assertIn('--no-fail-fast', call)
                if '--nocapture' in call:
                    streamed += 1
                else:
                    executions.append(call.split()[3])
        self.assertEqual(streamed, 1)
        self.assertEqual(sorted(executions), sorted(EXPECTED))

    def test_recording_helpers_force_lf_newlines(self):
        original = Path.write_text

        def windows_text(path, data, *args, **kwargs):
            kwargs.setdefault('newline', '\r\n')
            return original(path, data, *args, **kwargs)

        with patch.object(Path, 'write_text', windows_text):
            result, calls, _ = self.driver(1)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(calls[-1].startswith('test --no-fail-fast -p xtask'))

    def test_unsharded_keeps_workspace_compile_and_failures_collect(self):
        result, calls, summary = self.driver(fail_crate='haider-core')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls[1], 'test --workspace --no-run --locked')
        self.assertTrue(calls[-1].startswith('test --no-fail-fast -p xtask'))
        self.assertIn('simulated_failure', summary)


if __name__ == '__main__':
    unittest.main()
