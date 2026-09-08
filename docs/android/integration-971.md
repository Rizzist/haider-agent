# Embedded Android integration

The frozen interface remains [contracts-v1.md](contracts-v1.md). The workspace
version is still **0.0.970**, so the APK, JNI version, and UI fixtures use 0.0.970.

`MainActivity.AppContainer` owns `BinderRpcControlPlane` and `RpcDaemonService`.
The latter owns the display RPC connection, roster, replay cache, and
`RpcAccountsRepository`. `HaiderDaemonService` separately owns a View-only
`RpcSessionRosterSource` for notifications. Neither consumer opens the daemon
database. Binder replacement revokes the previous endpoint and snapshot sequence.

The first launcher action enables the foreground daemon. Later launcher actions
preserve an explicit Stop. OAuth and notification navigation do not enable a
disabled daemon. Automatic and explicit session selection wait for an
authoritative roster before making RPC calls. Active transcripts collect live
replay updates and retain Complete/Partial/Unavailable distinctions.
Cancellation requires an active coarse run state and its identity from the same
summary; a terminal run ID retained by the daemon does not expose Stop, count as
active in Settings, or place a seen session in the drawer's Active group.
The display cache recognizes lifecycle and ordinary metric records, retains
supported history-node text without duplicating its message, and replays old
projection cursors from zero. Unknown extensions, tools and attention records
remain partial rather than silently disappearing from coverage.

`JniNativeDaemonHost` calls the six C1 JNI methods. Native startup validates the
frozen paths/policy, receives the Keystore-unwrapped DEK, and injects
`EncryptedFileVault` and the standalone tool policy. The native workspace ceiling
is independent of provider lockdown bookkeeping. Desktop execution, credential
discovery/import, SSH, and peer routes remain unavailable in standalone mode.

API-key **Save** stages the key, clears the transient field/buffer and validates
and commits through `account.login_api`; there is no validate-only RPC.
Model and effort retries forward `confirm_new_epoch` only after the explicit
confirmation action supplied by UI round 7.
Round 10's permission picker reflects the production adapter's supported Ask
mode. Auto is disabled: the frozen RPC has no operation for changing standing
mobile consent on an existing session. The fake UI adapter's Auto simulation
does not grant production permission or suppress production input cards.
OAuth opens the daemon's authorization URL unchanged in an AndroidX Custom Tab,
with a package-scoped **Return to Haider** action. Both that action and the exact
`haider://oauth/return` relay open Settings. Real provider registration and browser
sign-in still require their own acceptance checks.

## UI parity merge

The UI round 12/parity tree (`18b2269c`) is integrated. Sending retains the
Ready/authoritative-roster wait and preserves edits made while startup or RPC is
pending. The chosen Steer/Queue delivery mode is forwarded to `turn.submit`.
Production CAS attachment staging and previews are not wired; a supplied
attachment is refused before submitting the turn, so text cannot be sent while
silently dropping a file. Queue management and usage return unavailable snapshots.
The new custom-server, fleet, workflow/Loom and checkpoint/branch surfaces retain
their facade's unavailable defaults until their production RPC adapters land.
Their fake/UI tests do not establish live support. The independent verifier must
keep these parity items separate from the embedded chat/lifecycle evidence.

## Build and verification

Use the repository toolchain and Android setup guide. On the shared mini, admit
every Cargo/Gradle/xtask invocation through
`/Users/rizzist/Developer/haiderharness/runtime/build-slot.sh <label> -- <command...>`
and use `--no-daemon` with one Gradle worker. That slot covers Gradle and its native
child. No load, other-process or nested admission heuristic is used. Prefix the
commands below with the slot helper.

```sh
cargo test --locked -p haider-android -p haider-rpc
cargo test --locked -p haider-accounts encrypted_file_vault
cargo test --locked -p haider-store store_synchronous --lib
cargo test --locked -p haider-daemon --no-default-features --features android-standalone android_ --lib -- --test-threads=1
gradle -p android --no-daemon --max-workers=1 :app:assembleRelease :app:assemblePhoneDebug :app:assembleEmulatorDebug :app:assemblePhoneDebugAndroidTest :app:assembleEmulatorDebugAndroidTest
gradle -p android --no-daemon --max-workers=1 :app:testPhoneDebugUnitTest :app:testPhoneReleaseUnitTest :app:testEmulatorDebugUnitTest :buildSrc:test
```

Gradle invokes `cargo ndk` for ARM64 and x86_64 at API 26, verifies both stripped
JNI inputs, and supplies only the selected ABI to each flavor. The Gradle native
task archives the unstripped ELF **and its packed `.dwp` companion** together under
the ELF build ID. `scripts/android/verify-native.py` checks the actual APK's ABI,
version, JNI exports, dependencies, and 16 KiB ELF/ZIP alignment. Existing release
signing configuration is unchanged; absent signing inputs produce an unsigned
release APK.

## Disposable fake-provider device gate

Use only a freshly reset Haider fixture on an emulator owned by the current
verification session. Before the first launcher start, use `run-as` to create an
empty file at `files/haider/runtime/android-default/fake-provider.enabled` with
mode 0600. Native code requires `FLAG_DEBUGGABLE`, the app UID, a regular file, and
one link. Release APKs cannot enable this fixture. No policy or Binder parameter
widens this gate.

The UI selects `fake / fake-model`. Only the provider implementation and its
route status are simulated; JNI, Binder, `h.sock`, session creation, worker
execution, storage, and transcript replay are real. Each turn returns:

> Haider integration fixture: the embedded daemon received your message over h.sock.

The fake provider does not claim a discovered model catalog or upstream HTTPS
acceptance. The device instrumentation verifies the actual reply in replay data.
The explicit fixture model `fake-needs-input` first asks a synthetic Continue
question through the real worker/menu pipeline, then returns the same reply.
Pass `expectedBuildId` to the instrumentation runner to compare loaded JNI
metadata with the packaged native source ID. An `adb am instrument` exit of zero
alone is not a passing test result; inspect its JUnit result.

Run actual Android CLI layout/ADB interactions and inspect every captured
screenshot on `Haider_API35`, stop that owned emulator, then repeat on
`Haider_API35_16KB`. Both mini images are ARM64; x86_64 compilation is not x86_64
device coverage. `android/tools/daemon_recovery_probe.py assert-ready` observes the
real service without instrumentation teardown. Do not use instrumentation as an
observer during a process-death/recovery experiment.

`mobile.sock` and standalone SMS/screen/accessibility transport remain a forward
integration item. The legacy token/loopback bootstrap is suppressed in standalone
mode. Passing chat over `h.sock` does not establish mobile capability transport,
real OAuth, physical OEM battery behavior, or publication readiness. Those remain
explicit verification scope for 971-V and the later embed/UI forward merges.
