# Android native contract diagnostic host

This independent Gradle project exercises the six frozen JNI v1 exports from
`haider-android`. It uses the `android-standalone` daemon feature and synthetic
vault contents in its own `ai.diffforge.haider.spike` package. The production
service, Binder adapters, and Compose app belong to other lanes.

Read the harness validation and Android setup guides before building. On the
shared Mac mini, use `CARGO_BUILD_JOBS=2`, require one-minute load below 8 for Cargo
and below 6 for Gradle/emulator admission, and serialize heavy work with other lanes. Never stop
another lane's emulator.

```sh
source /Users/rizzist/Developer/haiderharness/env.sh
cargo ndk -t arm64-v8a -t x86_64 -P 26 rustc -p haider-android --release --locked -- -Cstrip=none
```

Archive each unstripped library and its packed DWP by GNU build ID. Copy **stripped** libraries into `host/src/main/jniLibs/<abi>/libhaider.so`, then
run `gradle -p android/spike --no-daemon :host:assembleDebug :host:lintDebug`.
Audit both ELF files and APK alignment before installing. `nativeVersion` reports
a SHA-256 identity of Rust sources and Cargo manifests/lockfile; archive the ELF
GNU build ID and packaged-library SHA-256 alongside that source identity.

Start an available `Haider_API35` with `haider-emulator`, install the diagnostic
APK, enable CheckJNI, and launch `.SpikeActivity`. Use `android layout` and
actual `adb shell input tap` coordinates to operate these buttons:

1. **Start + Hello / Welcome**: tests malformed JNI input, array clearing,
   idempotent init, double start, a JCA-written HAV1 fixture rejected under a wrong
   DEK without changing ciphertext, then successful recovery with the matching
   synthetic key. Checks h.sock mode/owner, same-process peer PID, fragmented
   framing, Welcome metadata, and retained generation.
2. **Observe + inventory + mobile + TLS**: checks exact observation fields and
   100 nonblocking observations, creates a synthetic session and reads actual
   `tools.inventory`, verifies the non-token mobile.sock hello, and probes an
   HTTPS model endpoint without credentials. A typed unauthorized response is
   the HTTPS success criterion; transport errors are inconclusive failures.
3. **Inject TLS probe failure**: after a successful TLS probe, writes a fresh
   failed attempt and exercises cleanup; the earlier success must not survive.
4. **Shutdown + release**: checks actual shutdown status, h.sock drain/EOF,
   mobile.sock EOF, removal of both endpoints, and repeated release.
5. **Timeout + retain paused owner**: after the verifier confirms a tracer has
   stopped only the Rust owner thread, checks status 8, bounded release retaining
   the context, refused restart (status 1), and Java array clearing. The tracer
   must target only the disposable package and never inspect process memory.
   This uses the ordinary library without a native fault-injection feature.
6. **Forced shutdown + release**: after the verifier detaches that tracer,
   checks the retained join's actual status 9, Stopped observation, both removed
   sockets, and idempotent release.
7. **Run 100 lifecycle cycles**: repeats startup, observe/inventory/mobile and
   shutdown. The network probe runs separately, not 100 times. Summaries begin
   as non-passing and retain a unique run ID plus native identity.

Capture and visually inspect screenshots with Android CLI. Save `spike-*.json`
and `spike-events.log` through `adb shell run-as ai.diffforge.haider.spike`.
Each attempted vault/mobile/inventory/TLS record starts non-passing and includes
a run UUID, attempt UUID, native identity, and observed daemon generation. New
starts reset unattempted results.

Repeat on `Haider_API35_16KB`, one emulator at a time. An artifact-only x86_64
build does not prove x86_64 execution. This host does not prove service-process
restart, Binder death/rebind, Keystore wrapping, OEM power management, or the
production capability permission/UI journeys.

Native TLS uses bundled Mozilla/webpki roots, so it does not require the
rustls-platform-verifier Java companion. JNI reads ConnectivityManager's default
HTTP proxy before each start and supplies it explicitly to daemon/provider
clients, including the shared control client across restarts. Named OS proxies
resolve independently of the provider-origin DNS guard; direct requests retain
that guard. ProxyInfo's host exclusions use whole-host wildcard matching. The
adapter does not keep idle proxy connections across configuration changes. Emulator launch
uses the harness `-http-proxy http://127.0.0.1:10808`; an existing emulator may
instead expose `10.0.2.2:10808` as its Android global HTTP proxy. No process
environment is mutated. DNS-pinned web-fetch keeps its direct-client policy.
JNI resamples the configuration at nativeStart; live Android network-change
notifications and restart coordination require production-service integration.
PAC-only proxies are unsupported by this native HTTP adapter and fail startup
with a redacted error; this is not proof of PAC compatibility.

The JNI-independent `owner` module is the same implementation used on Android.
Host tests exercise its actual admission/release logic and consuming daemon join,
including a channel-held Tokio blocking reader across timeout and unwind. The
receipt-only tests remain complementary publication tests.

Raw lane evidence and the separate Astra verdict belong under the harness
`state/evidence/971-1/`, not Git. Preserve failed attempts and bind
final results to the exact candidate, APK, and ELF identities.
