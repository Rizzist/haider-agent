# Standalone Android lane 971-3

The frozen authority is `docs/android/contracts-v1.md`. These tasks require the integrated
`haider-android` crate and lane-2 service for packaging/device tests. A missing crate fails the
native task; no stub `.so`, skipped native task, or stale artifact is substituted.

```sh
source ~/.config/android/env.sh
python3 scripts/android/wait-for-build.py
CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo test -p haider-rpc --locked
python3 scripts/android/wait-for-build.py
gradle -p android :app:testPhoneDebugUnitTest :app:testPhoneReleaseUnitTest \
  :app:testEmulatorDebugUnitTest --no-daemon -Dorg.gradle.jvmargs=-Xmx3g --max-workers=2
python3 -m unittest discover -s scripts/android -p 'test_*.py'
```

Wait again before each build on the shared Mac. The guard polls every 60 seconds for at most
30 minutes, requiring load below 6, free+inactive memory above 3 GiB, and no other Gradle JVM.
The native child rechecks admission while excluding only its owning Gradle ancestors.

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
contents, uncompressed native entry and 16 KiB ZIP alignment. Release signing names and
v2/v3 verification remain in `android-apk.yml`; native symbols are a separate artifact.

## RPC integration boundaries

- Keep one process-scoped `RpcClient`; compose `StandaloneRpcConnection` with a Binder-owned
  `StateFlow<RpcTarget?>`. Revoking the endpoint closes the connection. Backgrounding or rotating
  the Activity does not close it. State is independent of native/service Ready.
- `SessionRosterRepository : SessionRoster` eagerly follows all pages, buffers the initial watch,
  applies complete SessionSummary replacements, and retains a read-only offline roster.
  The UI's lazy `loadMoreSessions` fake should adapt to the already fetched roster.
- `TranscriptRepository` attaches from the persisted applied cursor and reattaches gaps/lagged
  streams. `indexAll` covers all roster heads with single-envelope `session.read` ranges because
  wire v1 does not expose a typed oversized-page retry response. Unsupported visible payloads
  and oversized envelopes retain an explicit partial result. The display cache keeps allowed
  presentation fields; credentials, menu secret answers and account responses are never stored.
- UI `DaemonService` lifecycle/status remains lane 2's Binder facade; this package does not
  introduce another incompatible `DaemonService`. Bind session actions through `RpcMethods`,
  keeping worker/run/menu coordinates from one snapshot and command IDs across response-loss retries.
- `AccountsRepository : AccountsDataSource` matches the Settings operation names, with explicit
  semantic command IDs and a required OAuth desired alias. Adapt these in the UI facade.
  `validateApiKey` returns `validate_only_unavailable`: the frozen protocol only validates as
  part of `account.login_api`, which commits. A fake validate-only success is not a real wire door.
- The notifier uses its own View-only `RpcClient` plus the roster repository. Lane 2's
  `SessionRosterSource` adapter must map a freshly reconciled roster to `Baseline`, live summaries
  to `Changes`, and connection loss to `Reset`, projecting only its safe `NotificationSession`
  fields. This adapter and the UI facade wiring require the lane-2/UI types at integration.
- `LEGACY_TERMUX_TRANSPORT=false` blocks old preference reads/writes, capability startup and TCP
  connection startup in standalone. This lane does not edit the UI lane's onboarding/composables.

Rust wire fixtures are direct JVM resources. The only new fixture is generated from actual Rust
`EventPayload` types with:

```sh
UPDATE_ANDROID_GOLDEN=1 cargo test -p haider-rpc --test android_projection_golden_tests
```

## Advisory device tiers

`emulator-gate.py --apks DIR --tier pr|nightly|16k --evidence DIR [--serial DEVICE]` installs
exactly one debug app and its instrumentation APK, exercises real filesystem LocalSocket/JNI,
and requires the integrated deterministic fake-provider/Binder test. Missing service or fake
provider is a failure. The nightly tier actually reboots and enters Doze; the 16k tier requires
`getconf PAGE_SIZE == 16384`. Screenshot/layout evidence must then be visually inspected by Astra.
A successful script result covers only those probes, never full SHIP, live-provider OAuth,
physical OEM behavior, 24-hour memory, third-party order/payment, or all lifecycle cases.
The CI emulator jobs are advisory per registry #80 until a real first green is recorded.

On the Mac use only one owned `Haider_API35` emulator, after checking `adb devices -l`, and stop
it afterward. The Mac's emulator is ARM64; x86_64 execution and nightly image coverage belong to
CI. Do not install another lane's spike as if it were this candidate's native build.
