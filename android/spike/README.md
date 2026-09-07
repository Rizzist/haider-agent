# 971-1 embedding experiment

This independent Gradle project loads the real daemon from the new
`haider-android` cdylib. It does not participate in `android/settings.gradle.kts`
and does not modify the production app or release workflows.

The host uses synthetic key bytes and explicitly opts into
`{"spike_use_default_dependencies":true}`. Never use this APK with credentials:
the spike uses `DaemonDependencies::default()`, including the existing vault
and tool factory. It is an experiment, not the standalone security policy.

## Build and operate

Read the harness validation and Android setup guides first. On the shared mini,
before each cargo-ndk build or emulator start, wait for one-minute load below 6,
no Gradle Java process, and free+inactive memory above 3 GiB. Poll every 60 seconds
for at most 30 minutes, then proceed with two Cargo jobs as the lane directive
specifies. Serialize Cargo, Gradle, and emulator work; run one emulator at a time.

```sh
source ~/.config/android/env.sh
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
# The recorded lane build sets HAIDER_ANDROID_BUILD_ID to a native-source digest.
cargo ndk -t arm64-v8a -t x86_64 -P 26 build -p haider-android --release
mkdir -p android/spike/host/src/main/jniLibs/arm64-v8a
mkdir -p android/spike/host/src/main/jniLibs/x86_64
cp target/aarch64-linux-android/release/libhaider.so android/spike/host/src/main/jniLibs/arm64-v8a/
cp target/x86_64-linux-android/release/libhaider.so android/spike/host/src/main/jniLibs/x86_64/
gradle -p android/spike --no-daemon :host:assembleDebug :host:lintDebug
haider-emulator start Haider_API35
adb install -r android/spike/host/build/outputs/apk/debug/host-debug.apk
adb shell setprop debug.checkjni 1
adb shell am start -n ai.diffforge.haider.spike/.SpikeActivity
android layout --device=emulator-5554 --pretty
```

Use actual layout coordinates with `adb shell input tap` to start, observe,
stop, and run the lifecycle loop. Capture and inspect screenshots using the
Android CLI. Check runtime modes while Ready with `adb shell run-as
ai.diffforge.haider.spike ls -la files/haider/runtime/android-default`, then
capture the host's `spike-*.json` and `spike-events.log` files with `run-as`.
Always `haider-emulator stop` before starting the next AVD. Repeat on
`Haider_API35_16KB` and, when available, `Haider_API29`.

The test host checks Java-array clearing, malformed inputs, duplicate start,
path overflow, Ready, mode/UID, fragmented Hello framing, Welcome fields,
same-process peer PID, generation advancement, ServerDraining, EOF and socket
removal. The cycle button runs 100 init/start/route-query/stop/release cycles;
each expires the route cache to exercise ConnectivityManager through JNI on a
Tokio worker. This is a native lifecycle test, not a foreground-service or
Binder implementation. The native owner Java thread and both Tokio workers
reserve 8 MiB stacks.

Cycle summaries carry a unique run ID and native identity, start as non-passing,
and are updated after each completed cycle. For the stale-evidence regression,
retain app data after a successful loop, relaunch with
`adb shell am start -n ai.diffforge.haider.spike/.SpikeActivity --ei fail_cycle_at 2`,
then tap the cycle button. The new summary must say failed, passed=false, one
completed cycle and a different run ID. Save the earlier success separately.
The ordinary startup probe also supplies a throwing ContextWrapper to JNI;
under CheckJNI it must return INTERNAL and continue without a Java exception.

## Contract findings from source

- Rust 1.95's `File::lock`, `try_lock` and `unlock` return Unsupported on
  Android. The first device candidate failed before Ready at the lockdown
  quota lock; the profile lock would fail next. `haider-platform` now uses its
  existing rustix dependency's `flock` on Android, and preserves the std methods
  elsewhere. Only the daemon quota lock and store profile lock use this seam
  in this spike. Other credential-refresh/client/tool lock callers need a
  separate audit when those production paths are enabled. No Rust upgrade or
  dependency feature change is required. See the
  [pinned Rust source](https://github.com/rust-lang/rust/blob/1.95.0/library/std/src/sys/fs/unix.rs)
  and the actual Android CLI contention probe in the lane evidence.
- C1's byte queue must be at least **8,388,612 bytes**, because the existing
  daemon validates `frame_limit + 4`. This spike sets the minimum valid value;
  the frame body remains 8 MiB, connection cap 4, queue count 8, handshake 10 s,
  drain 5 s. Discovery is disabled and the lockdown root is explicit.
- On ARM64, BLAKE3's default NEON implementation contributes one exported Rust
  helper, `blake3_compress_in_place_portable`, beyond the six JNI exports.
  The new crate's linker map makes that helper local without disabling NEON or
  changing another crate. LLD may report the deliberate symbol reassignment;
  the ELF export gate verifies the actual six-symbol result. The crate also
  requests a GNU SHA-1 build ID so the stripped image and `.dwp` companion can
  be archived together by image identity. No dependency feature changes are
  needed for this experiment.
- `DaemonTask::readiness()` and `diagnostics()` do not expose the durable
  daemon generation. This spike obtains it by an internal real Hello/Welcome
  handshake once Ready. Add a typed generation field for production; do not
  retain this connection-consuming, potentially two-second observe workaround.
  Before recovery, the spike reports an unknown generation as JSON null.
  `nativeStart` returning OK means the task was spawned; store recovery and
  Ready happen asynchronously. Startup failure must remain observable after
  that return, rather than promising a synchronous recovery status code.
- `DaemonConfig::new` accepts the already profile-scoped runtime directory.
  On Unix, `Endpoint::new` appends only `h.sock`; it does not append a profile
  scope. Owner scoping may adjust a shared directory, so validate the resulting
  `config.runtime_dir` and publish the resulting endpoint.
- Android's socket path limit is 107 bytes **including the temporary bind
  path**, whose basename is `.hd-` plus 16 random characters. Therefore the
  runtime directory budget is **86 UTF-8 bytes**. Test host rejection covers a
  108-byte staging path whose final `h.sock` would otherwise fit.
- Kotlin `LocalSocket` creates its file descriptor lazily during `connect()`.
  Set `soTimeout` after connecting; setting it first throws `IOException:
  socket not created` before any Hello bytes can be sent. The real emulator
  probe exposed this ordering requirement after the native daemon reached Ready.
- A View-granted connection receives a `resident_session_binding` baseline
  after Welcome, including an explicit unbound baseline on an empty profile.
  The fixture consumes it and validates its generation before checking drain
  and EOF, matching the daemon's reference handshake tests. Production readers
  must dispatch unsolicited frames; `ServerDraining` may also be followed by
  queued checkpoints. This fixture creates no sessions, turns or checkpoints.
- A first `ShutdownHandle::request("android-forced")` only begins graceful
  drain. The spike calls `request_graceful()` then `request(...)` to force.
  `DaemonTask::join(self)` consumes the task: retain its future across timeout.
- Draft C1 names shutdown results without assigning integers. Spike mapping:
  Graceful/OK=0, Forced=9, Timeout=8; other status codes retain the draft values.
  NativeRelease waits for runtime threads, including blocking tasks, before
  releasing ndk-context and its application-context GlobalRef. A Java string
  allocation failure can return null; the final contract must define this case.
  The deadline currently bounds the consuming task join, not the subsequent
  blocking runtime destruction; it is not a total JNI-call wall-clock guarantee.
- `nativeInit` validates canonical, existing directories under the supplied
  application's real `filesDir`; Kotlin creates them first. No HOME resolver or
  temporary runtime fallback is used. Logs/tmp paths are validated but this
  spike does not implement production log rotation or a typed tmp/store-sync
  configuration seam.
- No `mobile.sock` implementation exists in this experiment. The current
  capability transport remains optional loopback TCP behind `HAIDER_MOBILE_APK`.
  The standalone lane needs a filesystem UDS acceptor with same-UID checks,
  removal of the token handshake, and a legacy transport feature boundary.
- The resolved reqwest rustls feature uses `rustls-platform-verifier` 0.7 on
  Android. Its TLS verification requires its own Android initialization and
  `org.rustls.platformverifier.CertificateVerifier` Kotlin dependency; merely
  initializing `ndk-context` does not satisfy that separate global state.
  This spike performs no upstream HTTPS request. The provider integration lane
  must wire and test that initialization or explicitly choose another trusted
  rustls root strategy; load/Ready/UDS success alone does not prove HTTPS.

## Production work deliberately outside the spike

Implement C3's encrypted vault injection and zeroizing native DEK ownership;
apply C4 to both advertised definitions and dispatch routes; replace ambient
store-sync/tmp configuration with typed inputs; implement mobile.sock, bounded
redacted logging and finalized JNI failure/status semantics. Test the private
daemon service, Binder, network changes, kill/recovery and Android lifecycle in
their assigned lanes. The installed API 29/35 devices do not prove API 26,
x86_64 runtime, physical OEM behavior or long-term reference-leak absence.

Build, ELF, APK and emulator results, with an evidence README and the separate
Astra verdict, are recorded under the harness `state/evidence/971-1/1-spike/`
directory. Generated native libraries, APKs and raw evidence stay out of Git.
