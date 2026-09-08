# Standalone Android lane 971-3

The frozen authority is `docs/android/contracts-v1.md`. These tasks require the integrated
`haider-android` crate and lane-2 service for packaging/device tests. A missing crate fails the
native task; no stub `.so`, skipped native task, or stale artifact is substituted.

```sh
source /Users/rizzist/Developer/haiderharness/env.sh
/Users/rizzist/Developer/haiderharness/runtime/build-slot.sh android-rpc -- cargo test -p haider-rpc --locked
/Users/rizzist/Developer/haiderharness/runtime/build-slot.sh android-jvm -- gradle -p android :app:testPhoneDebugUnitTest :app:testPhoneReleaseUnitTest \
  :app:testEmulatorDebugUnitTest --no-daemon -Dorg.gradle.jvmargs=-Xmx3g --max-workers=2
python3 -m unittest discover -s scripts/android -p 'test_*.py'
```

On the shared Mac, wrap every Cargo, Gradle and xtask invocation with the machine-wide
`runtime/build-slot.sh <label> -- <command...>` helper. It owns admission, quiet markers,
two build slots and Cargo resource limits. The Gradle slot covers its nested native task;
no separate load or process-count admission runs inside it. Isolated CI owns its runner.

`gradle -p android :app:assemblePhoneRelease :app:assembleEmulatorDebug` invokes
`buildHaiderNative` once, then copies the appropriate ABI to
`android/app/build/generated/haiderJniLibs/<variant>/<abi>/libhaider.so`.
The native task runs pinned cargo-ndk 4.1.2, NDK 28.2.13676358, Rust 1.95.0 and API 26 for
both ABIs. Cargo-ndk's environment linker wins over the API-26 compile-only fallbacks in
`.cargo/config.toml`; those fallbacks preserve the separately owned legacy xplat job.
All generated libraries/symbols stay under ignored build directories. Unstripped symbols are
archived by ELF build ID, while APK inputs are stripped and have a `.haider.build` provenance
section identifying the source digest, version, ABI and toolchain. This section proves the
build recipe's provenance; it is **not** a runtime `nativeVersion()` result. The instrumented
JNI test compares the actual native version with the APK version.

`verify-native.py --so ... --abi arm64-v8a --version ... --readelf ...` checks ELF type,
machine, LOAD alignment, RELRO, dependencies, JNI exports, build ID and provenance. Its
`--apk` form also needs `--aapt2`/`--zipalign`, and verifies the APK version, exact single-ABI
contents (`libhaider.so` and Compose’s `libandroidx.graphics.path.so`), uncompressed native
entries and 16 KiB ZIP alignment. Both libraries undergo ELF/DT_NEEDED checks; Haider alone
requires the embedded provenance and six JNI exports. Release signing names and
v2/v3 verification remain in `android-apk.yml`; native symbols are a separate artifact.

## RPC integration boundaries

- Keep one process-scoped `RpcClient`; compose `StandaloneRpcConnection` with a Binder-owned
  `StateFlow<RpcTarget?>`. Revoking the endpoint closes the connection. Backgrounding or rotating
  the Activity does not close it. State is independent of native/service Ready.
- `SessionRosterRepository : SessionRoster` eagerly follows all pages, buffers the initial watch,
  applies complete SessionSummary replacements, and retains a read-only offline roster.
  The UI's lazy `loadMoreSessions` fake should adapt to the already fetched roster.
  Account and roster watches register once per connection epoch; later refreshes reuse them.
  An RPC deadline closes that connection and surfaces an IO failure, preserving background recovery.
- `TranscriptRepository` attaches from the persisted applied cursor and reattaches gaps/lagged
  streams. `indexAll` covers all roster heads with single-envelope `session.read` ranges because
  wire v1 does not expose a typed oversized-page retry response. Unsupported visible payloads
  and oversized envelopes retain an explicit partial result. The display cache keeps allowed
  presentation fields; credentials, menu secret answers and account responses are never stored.
  Projection v2 rebuilds old cursors that may have discarded unknown item variants. Stale
  attachment IDs are ignored and a roster head rollback resets the affected replay cache.
- `RpcDaemonService : ui.daemon.DaemonService` composes the Binder `RpcControlPlane`
  port with the session, history and account repositories. `RpcAccountsRepository` supplies
  the UI's account interface on the same socket. See `docs/android/rpc-ui-integration.md`
  for constructor inputs, shared interface corrections and the remaining UI consumer work.
  `rosterReady` distinguishes initial hydration from an empty cache; `transcriptUpdates`
  folds pushed display events without reattaching per delta. Session operations acquire
  target Control authority and retain semantic IDs/original coordinates on retryable errors.
- `RpcClient` owns a 15-second Ping cadence and a 15-second write/Pong budget per epoch,
  below the daemon's 45-second read-idle limit. It echoes all valid u64 Ping nonces and
  requires a matching Pong. Heartbeats never restart a business-request deadline.
  Welcome frame ceilings are validated as positive u32 and clamped to the mobile limit.
- `validateApiKey` returns `validate_only_unavailable`: the frozen protocol only validates
  as part of `account.login_api`, which commits. OAuth keeps the caller's attempt ID and
  fences all transient references by the original connection epoch.
- The notifier uses its own View-only `RpcClient` plus the roster repository. Lane 2's
  `SessionRosterSource` adapter must map a freshly reconciled roster to `Baseline`, live summaries
  to `Changes`, and connection loss to `Reset`, projecting only its safe `NotificationSession`
  fields. This adapter and the UI facade wiring require the lane-2/UI types at integration.
- `LEGACY_TERMUX_TRANSPORT=false` blocks old preference reads/writes, capability startup and TCP
  connection startup in standalone. This lane does not edit the UI lane's onboarding/composables.

Rust wire fixtures are direct JVM resources. The only new fixture is generated from actual Rust
`EventPayload` types with:

```sh
UPDATE_ANDROID_GOLDEN=1 /Users/rizzist/Developer/haiderharness/runtime/build-slot.sh android-golden -- cargo test -p haider-rpc --test android_projection_golden_tests
```

## Advisory device tiers

`emulator-gate.py --apks DIR --tier pr|nightly|16k --evidence DIR --serial emulator-N --owned-emulator` installs
exactly one debug app and its instrumentation APK. The caller must own that disposable emulator.
Local APK package validation precedes every ADB call. Cleanup only stops an app installed by
this invocation and restores Doze only when this invocation forced it. The tier exercises real filesystem LocalSocket/JNI,
and requires the integrated deterministic fake-provider/Binder test. Missing service or fake
provider is a failure. The nightly tier actually reboots and enters Doze; the 16k tier requires
`getconf PAGE_SIZE == 16384`. Screenshot/layout evidence must then be visually inspected by Astra.
A successful script result covers only those probes, never full SHIP, live-provider OAuth,
physical OEM behavior, 24-hour memory, third-party order/payment, or all lifecycle cases.
The CI emulator jobs are advisory per registry #80 until a real first green is recorded.

On the Mac use only one owned `Haider_API35` emulator, after checking `adb devices -l`, and stop
it afterward. The Mac's emulator is ARM64; x86_64 execution and nightly image coverage belong to
CI. Do not install another lane's spike as if it were this candidate's native build.
