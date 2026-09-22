"""Android publication is a release gate even when desktop workflows are green."""
import contextlib
import importlib.util
import io
import os
import subprocess
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('android_asset', ROOT / 'scripts/release/require-android-asset.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
TAG, SHA = 'v0.0.971', 'a' * 40


def release():
    return dict(tagName=TAG, isDraft=False, url='https://example.test/release', assets=[
        dict(name=f'haider-{TAG}-android.apk{suffix}', size=100, state='uploaded')
        for suffix in ('', '.sha256')])


class AndroidReleaseAssetTest(unittest.TestCase):
    def test_requires_both_exact_uploaded_assets(self):
        self.assertEqual(len(gate.require_assets(release(), TAG)), 2)
        for field, value in [('size', 0), ('size', '100'), ('state', 'new'),
                             ('name', 'haider-v0.0.970-android.apk')]:
            for index in range(2):
                data = release()
                data['assets'][index][field] = value
                with self.subTest(field=field, index=index), self.assertRaises(ValueError):
                    gate.require_assets(data, TAG)
        for assets in ([], release()['assets'][:1], release()['assets'] * 2, None):
            data = release()
            data['assets'] = assets
            with self.assertRaises(ValueError):
                gate.require_assets(data, TAG)
        for field, value in [('tagName', 'v0.0.970'), ('isDraft', True)]:
            data = release()
            data[field] = value
            with self.assertRaises(ValueError):
                gate.require_assets(data, TAG)

    def run_gate(self, responses):
        with patch('sys.argv', ['gate', TAG, SHA, '--repo', 'owner/repo']), \
                patch.object(gate, 'gh_json', side_effect=responses) as gh, \
                contextlib.redirect_stdout(io.StringIO()) as out, \
                contextlib.redirect_stderr(io.StringIO()) as err:
            code = gate.main()
        return code, out.getvalue(), err.getvalue(), gh.call_args_list

    def test_queries_remote_tag_and_actual_release_assets(self):
        code, out, err, calls = self.run_gate([{'sha': SHA}, release()])
        self.assertEqual(code, 0, err)
        self.assertIn('PASS_ANDROID_RELEASE_ASSETS', out)
        self.assertEqual(calls[0].args, ('api', f'repos/owner/repo/commits/{TAG}'))
        self.assertEqual(calls[1].args, ('release', 'view', TAG, '--repo', 'owner/repo',
                                       '--json', 'tagName,isDraft,url,assets'))

    def test_remote_mismatch_api_failure_and_missing_apk_fail_loudly(self):
        missing = release()
        missing['assets'] = []
        for responses in ([{'sha': 'b' * 40}], [subprocess.CalledProcessError(1, ['gh'])],
                          [{'sha': SHA}, missing]):
            code, out, err, _ = self.run_gate(responses)
            self.assertEqual(code, 1)
            self.assertEqual(out, '')
            self.assertIn('RELEASE INCOMPLETE', err)

    def test_release_workflow_has_mandatory_post_upload_gate(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        publish = workflow.split('\n  publish:\n')[1].split('\n  npm:\n')[0]
        self.assertIn('scripts/release/require-android-asset.py', publish)
        self.assertNotIn('continue-on-error', publish)
        self.assertGreater(publish.index('scripts/release/require-android-asset.py'),
                           publish.index('gh release create'))


@unittest.skipIf(
    os.name == 'nt',
    'Android artifact collection is POSIX shell executed by the Ubuntu publish job',
)
class ReleaseShellTest(unittest.TestCase):
    """Execute the actual workflow shell; only GitHub transport uses fixtures."""
    def run_collection(self, scenario):
        import hashlib
        import json
        import tempfile
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        step = workflow.split('      - name: require verified Android APK artifact before publication\n')[1]
        script = step.split('        run: |\n', 1)[1].split('      - name:', 1)[0]
        script = '\n'.join(line[10:] for line in script.splitlines())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'bin').mkdir()
            (root / 'dist').mkdir()
            (root / 'fixture').mkdir()
            name = f'haider-{TAG}-android.apk'
            data = b'unit-test APK transport fixture; not a real APK'
            (root / 'fixture' / name).write_bytes(data)
            checksum = hashlib.sha256(data).hexdigest() if scenario != 'corrupt' else 'f' * 64
            (root / 'fixture' / (name + '.sha256')).write_text(f'{checksum}  {name}\n')
            if scenario == 'no-sidecar':
                (root / 'fixture' / (name + '.sha256')).unlink()
            runs = dict(workflow_runs=[dict(id=123, run_number=1, head_branch=TAG,
                        status='completed', conclusion='failure' if scenario == 'failed' else 'success')])
            if scenario == 'wrong-tag':
                runs['workflow_runs'][0]['head_branch'] = 'v0.0.970'
            (root / 'runs.json').write_text(json.dumps(runs))
            (root / 'bin/gh').write_text(r'''#!/bin/bash
set -euo pipefail
case "$*" in
  api\ *) cat "$FIXTURE_ROOT/runs.json" ;;
  'run download 123 --name haider-android-release --dir '*)
    [ "$SCENARIO" != absent ] || exit 1
    cp "$FIXTURE_ROOT/fixture/"* "${@: -1}/" ;;
  *) echo "unexpected gh command" >&2; exit 99 ;;
esac
''')
            (root / 'bin/sleep').write_text('#!/bin/bash\nexit 0\n')
            for name in ('gh', 'sleep'):
                (root / 'bin' / name).chmod(0o755)
            env = dict(os.environ, PATH=str(root / 'bin') + os.pathsep + os.environ['PATH'],
                       FIXTURE_ROOT=str(root), SCENARIO=scenario, RUNNER_TEMP=str(root / 'temp'),
                       GITHUB_REF_NAME=TAG, GITHUB_SHA=SHA, GITHUB_REPOSITORY='owner/repo')
            result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', script], cwd=root,
                                    env=env, capture_output=True, text=True, timeout=10)
            return result, sorted(path.name for path in (root / 'dist').iterdir())

    def test_collector_requires_actual_matching_complete_checksum_verified_artifact(self):
        result, files = self.run_collection('success')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(files, [f'haider-{TAG}-android.apk', f'haider-{TAG}-android.apk.sha256'])
        for scenario in ('failed', 'wrong-tag', 'corrupt', 'no-sidecar', 'absent'):
            result, files = self.run_collection(scenario)
            with self.subTest(scenario=scenario):
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn('::error::', result.stdout)
                self.assertEqual(files, [])
