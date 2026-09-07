#!/usr/bin/env python3
"""Advisory real-device tiers. Missing integration is a failure, never a synthetic pass."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

PACKAGE = 'ai.diffforge.haider'


def instrumentation_passed(text):
    return 'OK (' in text and 'FAILURES!!!' not in text and 'INSTRUMENTATION_FAILED' not in text


def aapt2_path():
    sdk = os.environ.get('ANDROID_HOME') or os.environ.get('ANDROID_SDK_ROOT')
    candidates = sorted((Path(sdk) / 'build-tools').glob('*/aapt2')) if sdk else []
    return shutil.which('aapt2') or (str(candidates[-1]) if candidates else 'aapt2')


def preflight(args):
    """Validate local artifacts and explicit device ownership before any ADB call."""
    if not args.owned_emulator or not re.fullmatch(r'emulator-[0-9]+', args.serial):
        raise RuntimeError('Requires --owned-emulator and an explicit emulator serial owned by this run')
    apks = sorted(args.apks.rglob('*.apk'))
    if len(apks) != 2:
        raise RuntimeError('Expected one test app and one instrumentation APK')
    packages = {}
    for apk in apks:
        badging = subprocess.check_output([args.aapt2, 'dump', 'badging', str(apk)], text=True)
        package = re.search(r"^package: name='([^']+)'", badging, re.MULTILINE)
        if package is None or package[1] in packages:
            raise RuntimeError('Invalid or duplicate APK package')
        packages[package[1]] = apk
    if set(packages) != {PACKAGE, PACKAGE + '.test'}:
        raise RuntimeError('Expected Haider app and Haider instrumentation packages')
    return packages[PACKAGE], packages[PACKAGE + '.test']


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument('--apks', type=Path, required=True)
    parser.add_argument('--tier', choices=['pr', 'nightly', '16k'], required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--owned-emulator', action='store_true', help='Confirm this run owns this disposable emulator')
    parser.add_argument('--aapt2', default=aapt2_path())
    args = parser.parse_args(argv)
    args.evidence.mkdir(parents=True, exist_ok=True)
    adb = ['adb', '-s', args.serial]
    steps = []

    def run(name, command, timeout=120, required=True, binary=False):
        started = time.monotonic()
        try:
            result = subprocess.run(command, capture_output=True, timeout=timeout)
            code, output = result.returncode, result.stdout + result.stderr
        except subprocess.TimeoutExpired as error:
            code, output = 124, (error.stdout or b'') + (error.stderr or b'')
        except OSError as error:
            code, output = 127, str(error).encode()
        (args.evidence / (name + ('.png' if binary else '.log'))).write_bytes(output)
        steps.append(dict(name=name, command=command, exit_code=code, seconds=round(time.monotonic() - started, 2)))
        if code and required:
            raise RuntimeError(f'{name} exited {code}')
        return output.decode(errors='replace')

    verdict, error = 'NO_SHIP', None
    installed_app = False
    forced_idle = False
    try:
        app, tests = preflight(args)
        devices = run('devices', ['adb', 'devices', '-l'])
        if not any(line.split()[:2] == [args.serial, 'device'] for line in devices.splitlines()):
            raise RuntimeError('Owned emulator is not online')
        page_size = run('page-size', adb + ['shell', 'getconf', 'PAGE_SIZE']).strip()
        if args.tier == '16k' and page_size != '16384':
            raise RuntimeError('16 KiB tier requires an actual 16384-byte page-size image')
        run('install-app', adb + ['install', '-r', str(app)])
        installed_app = True
        run('install-tests', adb + ['install', '-r', str(tests)])
        run('launch', adb + ['shell', 'am', 'start', '-W', '-n', PACKAGE + '/.MainActivity'])
        result = run('instrumentation', adb + ['shell', 'am', 'instrument', '-w', '-e', 'keepEnabled', 'true', PACKAGE + '.test/androidx.test.runner.AndroidJUnitRunner'], timeout=600)
        if not instrumentation_passed(result):
            raise RuntimeError('Instrumentation failed or produced no test completion')
        # Integration test doors must be present before this tier is called operational.
        service = run('services-before', adb + ['shell', 'dumpsys', 'activity', 'services', PACKAGE])
        if 'HaiderDaemonService' not in service or 'isForeground=true' not in service:
            raise RuntimeError('Standalone foreground service is not active; integrate lane-2 lifecycle fixture')
        run('runtime-metadata', adb + ['shell', 'run-as', PACKAGE, 'ls', '-ld', 'files/haider/runtime/android-default/h.sock', 'files/haider/runtime/android-default/mobile.sock'])
        if args.tier == 'nightly':
            run('reboot', adb + ['reboot'])
            run('wait-device', adb + ['wait-for-device'], timeout=180)
            deadline = time.monotonic() + 180
            while time.monotonic() < deadline:
                if run('boot-completed', adb + ['shell', 'getprop', 'sys.boot_completed']).strip() == '1':
                    break
                time.sleep(2)
            else:
                raise RuntimeError('Real emulator reboot timed out')
            run('unlock', adb + ['shell', 'input', 'keyevent', '82'])
            service = run('services-after-reboot', adb + ['shell', 'dumpsys', 'activity', 'services', PACKAGE])
            if 'HaiderDaemonService' not in service or 'isForeground=true' not in service:
                raise RuntimeError('Enabled daemon did not recover after reboot')
            idle = run('idle-before', adb + ['shell', 'dumpsys', 'deviceidle'])
            if 'mForceIdle=true' in idle:
                raise RuntimeError('Emulator was already forced idle; refusing to change prior state')
            forced_idle = True  # Also restore if force-idle times out after changing state.
            run('doze', adb + ['shell', 'dumpsys', 'deviceidle', 'force-idle'])
            idle = run('doze-state', adb + ['shell', 'dumpsys', 'deviceidle'])
            if 'mState=IDLE' not in idle:
                raise RuntimeError('Device did not enter real Doze')
            run('undo-doze', adb + ['shell', 'dumpsys', 'deviceidle', 'unforce'])
            forced_idle = False
        verdict = 'PASS_TIER_CHECKS'
    except Exception as failure:
        error = str(failure)
    finally:
        if forced_idle:
            run('restore-idle', adb + ['shell', 'dumpsys', 'deviceidle', 'unforce'], required=False)
        if installed_app:
            run('processes', adb + ['shell', 'ps', '-A'], required=False)
            run('service-state', adb + ['shell', 'dumpsys', 'activity', 'services', PACKAGE], required=False)
            # Crash buffer only. Never capture vault, account state or unrestricted RPC/product logs.
            run('crashes', adb + ['logcat', '-b', 'crash', '-d'], required=False)
            run('layout-dump', adb + ['shell', 'uiautomator', 'dump', '/sdcard/haider-test-layout.xml'], required=False)
            run('layout-pull', adb + ['pull', '/sdcard/haider-test-layout.xml', str(args.evidence / 'layout.xml')], required=False)
            run('screen', adb + ['exec-out', 'screencap', '-p'], timeout=30, required=False, binary=True)
            run('stop-test-app', adb + ['shell', 'am', 'force-stop', PACKAGE], required=False)
            if any(step['exit_code'] for step in steps if step['name'] in {'layout-dump', 'layout-pull', 'screen', 'stop-test-app'}):
                verdict = 'NO_SHIP'
                error = error or 'Required evidence capture or owned-app cleanup failed'
        (args.evidence / 'result.json').write_text(json.dumps(dict(verdict=verdict, tier=args.tier, error=error, steps=steps,
            screenshot_inspection='pending human/Astra visual inspection' if installed_app else 'not captured: no candidate installed',
            coverage='tier probes only; physical OEM/live-provider flows remain separate'), indent=2))
    if error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
