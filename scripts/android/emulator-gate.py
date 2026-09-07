#!/usr/bin/env python3
"""Advisory real-device tiers. Missing integration is a failure, never a synthetic pass."""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import time

PACKAGE = 'ai.diffforge.haider'


def instrumentation_passed(text):
    return 'OK (' in text and 'FAILURES!!!' not in text and 'INSTRUMENTATION_FAILED' not in text


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--apks', type=Path, required=True)
    parser.add_argument('--tier', choices=['pr', 'nightly', '16k'], required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--serial')
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, exist_ok=True)
    adb = ['adb'] + (['-s', args.serial] if args.serial else [])
    steps = []

    def run(name, command, timeout=120, required=True):
        started = time.monotonic()
        try:
            result = subprocess.run(command, capture_output=True, timeout=timeout)
            code, output = result.returncode, result.stdout + result.stderr
        except subprocess.TimeoutExpired as error:
            code, output = 124, (error.stdout or b'') + (error.stderr or b'')
        (args.evidence / (name + '.log')).write_bytes(output)
        steps.append(dict(name=name, command=command, exit_code=code, seconds=round(time.monotonic() - started, 2)))
        if code and required:
            raise RuntimeError(f'{name} exited {code}')
        return output.decode(errors='replace')

    verdict, error = 'NO_SHIP', None
    try:
        devices = run('devices', ['adb', 'devices', '-l'])
        if not args.serial and len([line for line in devices.splitlines() if '\tdevice' in line or ' device ' in line]) != 1:
            raise RuntimeError('Select exactly one disposable emulator using --serial')
        page_size = run('page-size', adb + ['shell', 'getconf', 'PAGE_SIZE']).strip()
        if args.tier == '16k' and page_size != '16384':
            raise RuntimeError('16 KiB tier requires an actual 16384-byte page-size image')
        apks = list(args.apks.rglob('*.apk'))
        app = [p for p in apks if 'androidTest' not in str(p) and 'androidtest' not in p.name.lower()]
        tests = [p for p in apks if p not in app]
        if len(app) != 1 or len(tests) != 1:
            raise RuntimeError('Expected one test app and one instrumentation APK')
        run('install-app', adb + ['install', '-r', str(app[0])])
        run('install-tests', adb + ['install', '-r', str(tests[0])])
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
            run('doze', adb + ['shell', 'dumpsys', 'deviceidle', 'force-idle'])
            idle = run('doze-state', adb + ['shell', 'dumpsys', 'deviceidle'])
            if 'mState=IDLE' not in idle:
                raise RuntimeError('Device did not enter real Doze')
            run('undo-doze', adb + ['shell', 'dumpsys', 'deviceidle', 'unforce'])
        verdict = 'PASS_TIER_CHECKS'
    except Exception as failure:
        error = str(failure)
    finally:
        run('restore-idle', adb + ['shell', 'dumpsys', 'deviceidle', 'unforce'], required=False)
        run('processes', adb + ['shell', 'ps', '-A'], required=False)
        run('service-state', adb + ['shell', 'dumpsys', 'activity', 'services', PACKAGE], required=False)
        # Crash buffer only. Never capture vault, account state or unrestricted RPC/product logs.
        run('crashes', adb + ['logcat', '-b', 'crash', '-d'], required=False)
        run('layout-dump', adb + ['shell', 'uiautomator', 'dump', '/sdcard/haider-test-layout.xml'], required=False)
        run('layout-pull', adb + ['pull', '/sdcard/haider-test-layout.xml', str(args.evidence / 'layout.xml')], required=False)
        screen = subprocess.run(adb + ['exec-out', 'screencap', '-p'], capture_output=True, timeout=30)
        (args.evidence / 'screen.png').write_bytes(screen.stdout)
        run('stop-test-app', adb + ['shell', 'am', 'force-stop', PACKAGE], required=False)
        (args.evidence / 'result.json').write_text(json.dumps(dict(verdict=verdict, tier=args.tier, error=error, steps=steps,
            screenshot_inspection='pending human/Astra visual inspection', coverage='tier probes only; physical OEM/live-provider flows remain separate'), indent=2))
    if error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
