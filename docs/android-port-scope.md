# Android port — what actually needs doing

Scoped 2026-08-24 against v0.0.950 by reading every platform-conditional site.
Written down so the port starts from evidence rather than re-deriving it.

## The headline: 46 is the wrong number

A raw sweep finds 46 `target_os = "linux"` sites and that reads as a large port.
It isn't. Split by what the code does:

| Area | Sites | Bearing on Android |
|---|---|---|
| `haider-tools/src/computer.rs` | 19 | computer-use: X11/Wayland capture + input. **Phone-use wave, not the base port.** |
| `haider-platform/src/ipc/unix.rs` | 8 | socket anchoring — base port |
| `haider-platform/src/process.rs` | 4 | process-group liveness via `/proc` — base port |
| `haider-tools/src/bin/haider-wayland-portal.rs` | 3 | Linux desktop only; **irrelevant to Android** |
| `haider-client/src/profile.rs` | 3 | runtime/profile paths — base port |
| openai.rs, Cargo.toml, tests | 8 | trivial or test-only |

**Base port is ~15 sites.** The other 31 are either a separate wave or don't apply.

## The port is mostly widening cfgs, not writing implementations

Most Linux gates guard *Linux-like* behaviour that Android also has. Android runs
a Linux kernel: it has `/proc`, POSIX process groups, and unix domain sockets.
Gating those on `target_os = "linux"` excludes Android from implementations that
would be **more correct for it** than the fallbacks it currently lands in.

The codebase already contains the idiom, at `ipc/unix.rs:284`:

```rust
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
```

So `target_os = "android"` has appeared here before. Much of this work is
finishing a pattern that was started.

## Blockers found, with evidence

### 1. `/bin/sh` is hardcoded — hooks fail outright

Hook execution hardcodes `/bin/sh` under `cfg(unix)` in `hooks.rs`. Android ships
`/system/bin/sh`; there is no `/bin/sh`. Every hook would fail.

Found while fixing a **Windows** test fixture during 950 — nobody was looking for
Android problems. Had CI stayed red, this would have surfaced mid-port and been
blamed on the port.

### 2. `/tmp` is hardcoded — the daemon cannot start

`profile.rs:210-220`:

```rust
fn runtime_dir(env: &ProfileEnv) -> PathBuf {
    #[cfg(target_os = "linux")]
    if let Some(xdg) = &env.xdg_runtime_dir && verified_owner_private(xdg) {
        return xdg.join("haider");
    }
    #[cfg(unix)]
    return PathBuf::from("/tmp").join(format!("haider-{}", effective_uid()));
}
```

Android is not `target_os = "linux"`, so it falls to `/tmp` — which **does not
exist on Termux**. Termux uses `$PREFIX/tmp`, exposed as `$TMPDIR`. The daemon
fails to create its runtime directory and never starts.

Fix: honour `$TMPDIR` (and `$PREFIX`) rather than assuming `/tmp`. Note this also
makes the macOS/BSD path more correct, since `/tmp` is an assumption there too.

### 3. `/proc` scanning excludes Android

`process.rs:467-520` gates `process_group_exists` and
`linux_process_group_has_live_member` (which reads `/proc`) on
`target_os = "linux"`, with a `cfg(all(unix, not(target_os = "linux")))`
fallback. **Android has `/proc`.** It currently takes the macOS/BSD fallback.

### 4. Socket anchoring excludes Android

`ipc/unix.rs:27-262` gates `SocketAnchor`/`anchor_socket` on
`target_os = "linux"`, with a no-op struct otherwise. Android supports the same
primitives; it currently gets the degraded path.

## The dangerous property

**Android currently compiles.** It falls into `not(target_os = "linux")` branches
that exist and build. So an Android target could go green while silently using
macOS/BSD implementations on a Linux-kernel platform.

That is the same class as the macOS-only release gate that made deterministic
Linux failures look like a flake for weeks: it builds, it runs, and it is quietly
wrong. **A green Android build is not evidence the port is correct** — every
widened cfg needs a reason stated for why the Linux implementation is right under
Bionic, not merely that it compiles.

## Sequencing

`xplat-check` must be genuinely green first. Android is a Linux variant; porting
onto a Linux target whose tests are failing produces a green Android build that
proves nothing. As of v0.0.950 three `server_mode_*` tests still fail on Windows
and the Linux job's status is unconfirmed (its log was unretrievable).

## Phone-use is a different project

Screen capture and input injection cannot come from Termux:
`MediaProjection` needs a foreground service and user consent, and input
injection needs an `AccessibilityService` declared in an APK manifest and
manually enabled. Termux:API provides SMS, clipboard, camera, location,
notifications, telephony, TTS, vibrate and sensors — real capability, but not
see-and-act.

So phone-use needs a companion APK, which is a Kotlin/Java project with its own
permissions UX, Play-policy constraints (SMS and AccessibilityService will not
pass review; sideload or F-Droid is the path), and a security review of the
localhost surface the Diff Forge PWA would reach — a browser cannot open a unix
socket, so that surface is new, not a transport swap.

---

# The Android concept: what we are actually building

Written 2026-08-24 as the architectural frame for the phases below. Nothing here
is implemented — this records the shape and the reasoning so the first lane does
not have to re-derive it.

## Two artifacts, two pipelines

These get conflated and should not be:

| | Toolchain | Output | Installs into |
|---|---|---|---|
| CLI + daemon | Rust, `aarch64-linux-android`, NDK | tarball of two binaries | Termux |
| Companion app | Gradle, Android SDK | APK | Android directly |

CI can produce both, as separate jobs. The first is a fifth entry in the
existing release matrix. The second is a new project with its own language,
review process and store policy.

**AAB is probably never needed.** AAB is a Play Store format, and Play will
reject SMS permissions and an AccessibilityService used for automation — their
policy requires accessibility services to be *for* accessibility. Sideloaded APK
or F-Droid is the distribution path. That is how Termux itself ships.

## The companion app needs UI, for structural reasons

Not product UI — Android forces it:

- runtime permissions can only be requested from an Activity; there is no
  headless way to ask
- long-running background work requires a foreground service, which requires a
  persistent notification
- **an AccessibilityService cannot be enabled programmatically at all.** The
  user must open Settings and turn it on. Without guidance nobody completes it.

So the minimum is one screen: permission states with grant actions, a service
on/off control, and the daemon's local address. A control panel, not an app.
The real UI is the PWA; the APK is a permission broker and service host.

## The design question, and the answer

**How does a Rust daemon reach Android capabilities it has no permission to
touch?** Three options:

1. **Embed the daemon in the APK via JNI.** Cleanest capability access, but
   couples the Rust to Android's lifecycle and makes Termux a second
   implementation.
2. **APK as a capability broker over a local socket.** ← RECOMMENDED. The daemon
   asks for "screenshot", "tap at x,y", "send SMS"; the APK executes with its
   granted permissions and returns a result. The daemon stays a plain Unix
   process and does not know it is on Android.
3. **Shell out to `termux-*`.** Works with no APK, but caps at what Termux:API
   declares — which excludes screen capture and input injection, i.e. everything
   that makes phone-use *phone-use*.

The broker wins because **it mirrors what computer-use already does**: a tool
interface with a platform-specific implementation behind it. Phone-use becomes
another backend rather than a new subsystem, and Termux and the APK can coexist
— Termux for development, APK for the product — without two daemons.

## Phases

**Phase 1 — the port.** Daemon and CLI run under Termux. No Android
capabilities. Proves `/proc`, sockets, process groups and paths work under
Bionic. This is the ~15 sites above.

**Phase 1 status.** An `aarch64-linux-android` CI full-link build now proves the
CLI and daemon compile against the NDK; process tools resolve a Termux shell
through executable `$SHELL` or PATH when `/bin` shells are absent, and Android
excludes cpal behind an honest `MicUnavailable` capture stub. The no-op
socket-inode anchor and kill-probe process-group check (rather than `/proc`
member scanning) remain intentional conservative Unix fallbacks for Phase 1.

**Phase 2 — the broker.** APK ships permissions, a foreground service and a
capability socket. Daemon gains phone-use tools. SMS, screenshot and input
injection arrive here.

**Phase 3 — the PWA.** Diff Forge on the phone talks to the local daemon. Needs
a localhost surface with origin checks, because a browser cannot open a unix
socket. That is a new security surface, not a transport swap.

## The failure mode to guard against

**Do not treat a green Phase 1 build as evidence the port works.** Android
compiles today by falling into `not(target_os = "linux")` branches — it would
build, run, and be quietly wrong. Every widened cfg needs a stated reason why
the Linux implementation is correct under Bionic, not merely that it compiles.
