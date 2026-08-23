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
