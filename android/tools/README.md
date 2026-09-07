# Service recovery probes

Use `python3 android/tools/daemon_recovery_probe.py --serial <owned-device> snapshot`
or `assert-ready` after an external reboot, scoped daemon PID kill, or replacement.
The service's `dumpsys activity service` output contains only lifecycle diagnostics.
It does not create/bind the service, run instrumentation, or finish an instrumentation session.
`assert-disabled` reads only the debug fixture's persisted lifecycle state using `run-as`.
A missing state file is an error; use this assertion after a user Stop has persisted state.

The former `DaemonRecoveryProbeTest` instrumentation class was removed: ordinary
instrumentation can stop the whole target package when it starts or finishes.
Do not use the remaining JNI/Keystore instrumentation suite as an observer of a
concurrently running recovery matrix. Run it only on a disposable device owned by
that test session; missing native libraries remain failures.

Observation needs no teardown. When the fixture session owns the service, the
explicit `cleanup` action stops only `HaiderDaemonService` through its own UID.
This fixture targets Android user 0; the command passes `am stopservice --user 0`
explicitly because `run-as` cannot use Activity Manager's cross-user `current` default.
Android can return status 255 even when it prints `Service stopped`. Cleanup
preserves the raw command exit in JSON, accepts only the exact stopped/already-absent
messages, and confirms service absence through dumpsys. Permission errors, unknown
output and a service that remains present are failures.
It retains the enabled preference, as framework destruction does. To revoke
opt-in, use the app's notification Stop before cleanup. Never use `am force-stop`
or broad PID kills for this matrix, and never clean up another lane's device.

Run the host regression checks with
`python3 -m unittest discover -s android/tools -p 'test_*.py'`.
