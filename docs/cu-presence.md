# Computer-use presence: "Haider is controlling …" and Stop

Owner request (2026-09-24): when Haider drives a screen, phone or browser, the
human must **see** it and be able to **stop** it, the way ChatGPT/Codex agent
mode does (Chrome's `"ChatGPT" started debugging this browser · Cancel` bar,
a coloured agent tab group, a visible agent pointer).

This document is the design and the integration contract. Code:

| Piece | Location |
| --- | --- |
| Platform-neutral state machine, wire vocabulary, overlay art | `crates/haider-tools/src/presence.rs`, `presence/art.rs` |
| macOS overlay helper | `crates/haider-tools/src/presence/overlay_macos.rs` |
| Windows overlay helper | `crates/haider-tools/src/presence/overlay_windows.rs` |
| Linux notification helper (X11 and Wayland) | `crates/haider-tools/src/presence/overlay_linux.rs` |
| Daemon controller, renderers, Stop → cancel | `crates/haider-daemon/src/cu_presence.rs` |
| Dispatcher hooks (computer/mobile routes, run end) | `crates/haider-daemon/src/worker.rs` (`cu_presence` field) |
| APK ↔ daemon frames | `crates/haider-daemon/src/mobile_transport.rs` (`presence.stop`, `presence.end`) |
| Android overlay, chip, notification | `android/app/src/main/java/ai/diffforge/haider/service/presence/` |
| TUI header chip | `crates/haider-tui/src/projection.rs` (`cu_presence`), `render.rs` |
| Browser extension (webextract-2) | `browser/haider-presence-extension/` |

## 1. What "Haider is controlling" means

Presence is **run-scoped** and keyed by `session_id/run_id` (one dispatcher per
accepted run).

* **Appears** on the run's first CU action on a surface: any `computer`
  action (observation included, so a screenshot-only run is still visible),
  or any `mobile` action that operates the phone's screen (not `sms_read` /
  `list_apps`).
* **Follows** every later action: the agent pointer animates to the target
  and marks clicks (ring), drags, scrolls and typing (caption).
* **Disappears** when the run ends (completed, errored or cancelled — the
  dispatcher's `close_effects`), when Stop is pressed, or after
  `CU_PRESENCE_IDLE_SECS` = **30 s** without a CU action (never while an
  action, e.g. a 60 s `wait`, is still in flight). The next action re-shows
  it. The constant lives in `haider_protocol::computer` and is mirrored by the
  TUI and the Android controller (`CU_PRESENCE_IDLE_MS`).

Surfaces are independent: stopping the phone does not stop a concurrent
desktop run, and vice versa.

## 2. Stop, end to end

```
human clicks Stop (desktop badge | phone chip/notification | Chrome infobar Cancel)
  → surface emits {"event":"stop"}            (desktop helper stdout / APK push / extension message)
  → CuPresence::stop(surface)                  (haider-daemon/src/cu_presence.rs)
      1. PresenceMachine::stop marks every active lease on that surface stopped
      2. the in-flight action's cancel token is flipped at once
         (ComputerCancelToken / MobileCancelToken → the backend aborts typing,
          waits, gestures between batches)
      3. the run's stop hook submits TurnCancelCommand
         {command_id:"cu-presence-stop-<run>", reason:"cu-presence-stop"}
         via SessionHub::cancel_internal_turn — the same receipt-backed
         cancellation authority as the TUI's ESC (`turn.cancel`)
      4. the surface shows "…" then hides
  → core cancels the turn → dispatcher close_effects(cancelled=true)
      → emergency_stop releases held buttons, broker journals EffectOutcome::Cancelled
      → PresenceLease::end(cancelled)
```

The provider can race ahead and request another action between the
in-flight cancel and the turn cancellation landing. Such an action is refused
right after authorization, before preflight and dispatch (the same shape as a
failed preflight), and an action already past that point is refused before
`execute` (`PresenceRefusal::Stopped` → journaled `Cancelled`); either way
nothing of a stopped run reaches the OS. A new run may control the surface
again.

**Focus.** Stop never needs keyboard focus and never takes it from the app
being driven: the macOS badge is a non-activating `NSPanel` owned by an
accessory (no Dock icon) process; the Windows badge answers
`WM_MOUSEACTIVATE` with `MA_NOACTIVATE`; the Android chip is
`FLAG_NOT_FOCUSABLE`; the Chrome infobar is browser chrome. ESC in the TUI
remains a second Stop.

**The agent cannot press its own Stop.** Every mouse event Haider synthesises
carries `SYNTHETIC_INPUT_TAG` (macOS `kCGEventSourceUserData`, Windows
`dwExtraInfo`); overlay helpers ignore a Stop click carrying it. In addition,
before any pointer-input action the overlay moves the badge to the opposite
screen edge if the target is within 28 pt of it, and acknowledges the pointer
command; the daemon waits for that ack for at most `POINTER_ACK_TIMEOUT`
(250 ms) so the real click lands under the drawn arrow but a slow or dead
overlay can never block control. Android has no event tagging for
accessibility gestures, so the chip always relocates away from a tap/swipe
target before the gesture is dispatched.

## 3. Desktop overlay

`haiderd` re-executes itself as `haiderd --cu-presence-overlay` (no new
release member, installer entry or signing identity). The daemon spawns it on
the first `Show`, speaks line-delimited JSON on stdin/stdout, and closes stdin
on the final `Hide`; stdin EOF (including daemon death) makes the helper exit,
so an orphaned indicator is impossible. `HAIDER_CU_PRESENCE=0` disables the
overlay (headless/CI); `HAIDER_CU_PRESENCE_HELPER=/abs/path` selects another
helper. A test binary embedding the daemon never spawns one.

Wire (`haider_tools::presence`):

```json
{"op":"show","surface":"screen","label":"Haider is controlling this screen"}
{"op":"pointer","surface":"screen","seq":7,"mark":"click","point":{"x":812.0,"y":344.5}}
{"op":"stopping","surface":"screen"}
{"op":"hide","surface":"screen","reason":"run_ended|cancelled|idle|stopped"}
```
```json
{"event":"ready","platform":"macos","capture_excluded":true}
{"event":"ack","seq":7}
{"event":"stop"}
{"event":"error","message":"…"}
```

`point` is in global desktop coordinates with a top-left origin (Quartz
points on macOS, virtual-screen pixels on Windows), produced by the backend's
own `presence_point` mapping from the delivered screenshot pixel, so the
arrow lands exactly where the input will.

The art (gold arrow with dark outline and white halo, click ring, dark pill
badge with red live dot and red **Stop** button) is rasterised once in
`presence/art.rs` and shared by all desktop renderers.

### Screenshot exclusion (the model's view is unaffected)

| Platform | Mechanism | Deterministic? |
| --- | --- | --- |
| macOS | every panel sets `NSWindowSharingNone`; the backend captures with `CGDisplayCreateImage`, which omits sharing-none windows | yes — verified live on macOS 26 (Darwin 25.6): with the overlay on screen (window server lists it at level 1000, sharing state 0) the model-facing CAS image contains neither pointer nor badge, and `screencapture` omits it too |
| Windows | `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` on every overlay window (Windows 10 2004+), which Windows documents as excluding the window from capture | by the OS contract on 2004+ — **not runtime-verified here** (cross-compiled and Clippy-checked only); `ready.capture_excluded=false` is reported when the OS refuses |
| Linux | no pointer overlay (below) | n/a |

The overlay is click-through everywhere except the badge (`ignoresMouseEvents`
/ `WS_EX_TRANSPARENT`).

Because sharing-none also hides the overlay from every screenshot and screen
recording (that is the point), documenting what the human sees needs
`HAIDER_CU_PRESENCE_EVIDENCE_CAPTURABLE=1`, an evidence-only switch that uses
`NSWindowSharingReadOnly` and reports `ready.capture_excluded=false`. Observed
on this Mac: even in that mode the daemon's own `CGDisplayCreateImage` capture
still omitted the overlay (only `screencapture` showed it), apparently because
the helper is the capturing process's own child; production does not rely on
this and keeps sharing-none.

### Linux

* **Wayland**: a client cannot create a global always-on-top surface or know
  global coordinates. `wlr-layer-shell` exists on wlroots compositors and KDE
  but not GNOME, and the xdg-desktop-portal ScreenCast stream the backend
  captures offers no per-surface exclusion, so a layer-shell pointer would
  appear in the model's screenshots.
* **X11**: an override-redirect window is easy to draw, but X11 has no
  capture-exclusion primitive: the backend's `GetImage` of the root window
  would include it. Hiding it around each capture relies on other clients
  repainting exposed areas in time and is not deterministic.
* **Implemented fallback (both)**: a resident desktop notification
  (`org.freedesktop.Notifications`) titled "Haider is controlling this
  screen", updated with the latest action, with a **Stop** action wired to the
  same Stop path. Honest limit: a notification popup is an ordinary window and
  can appear in a screenshot taken while it is on screen (it is posted with
  low urgency so desktops retire the popup into the notification list, where
  Stop remains available). No agent pointer is drawn on Linux.

## 4. Android

`HaiderAccessibilityService` attaches `AccessibilityPresenceOverlay`:

* a full-screen `TYPE_ACCESSIBILITY_OVERLAY` layer (`FLAG_NOT_TOUCHABLE`)
  drawing the same gold arrow, caption and tap ripple at each
  `a11y.tap`/`a11y.swipe` target;
* a "Haider is controlling your phone · Stop" chip (`FLAG_NOT_FOCUSABLE`)
  that relocates away from gesture targets;
* an ongoing low-importance notification with a **Stop** action
  (`PresenceStopReceiver`, non-exported, `:daemon` process). Without
  `POST_NOTIFICATIONS` the chip remains the Stop affordance.

`CuPresenceController` (pure Kotlin) mirrors the machine: every capability
request that operates the screen raises/refreshes it; `presence.end` from the
daemon or 30 s of silence retires it. Stop hides it, latches mutating requests
off (`rejected: stopped_by_user`) until `presence.end` or 10 s, and emits
`{"type":"presence.stop"}` as an APK push; the daemon routes that push to
`CuPresence::stop(Phone)`. When the phone lease hides for any reason other
than Stop the daemon sends the `presence.end` request (old APKs answer
`unsupported_request`, which is ignored).

Capture exclusion: MediaProjection records accessibility overlays, so the
`screen.capture` handler runs the capture inside
`CuPresence.withOverlayHidden`: both overlay views go `INVISIBLE`, the helper
waits two vsyncs plus the view's frame-commit callback (API 29+), and only
then does `ScreenCaptureService` create its virtual display (the first frame
it receives is composed after the hidden frame). The views are restored
afterwards. On the 16 KiB/API 35 targets this is a ~2–3 frame hide.

Transport note: the only APK capability transport in the tree today is the
legacy Termux `TransportClient`, which forwards `CuPresence.stops`. The
standalone APK-side `mobile.sock` client does not exist yet; when it lands it
must forward `CuPresence.stops` the same way (one line).

## 5. Browser (contract for webextract-2)

**Decision: attach through the `chrome.debugger` extension API, not a CDP
remote-debugging launch.**

* `chrome.debugger.attach` makes Chrome itself show
  `"Haider" started debugging this browser  [Cancel]` — un-spoofable browser
  chrome, exactly the owner's reference, and Cancel is a native Stop.
* A `--remote-debugging-port/pipe` launch shows either nothing (no signal to
  the human) or, with `--enable-automation`, "Chrome is being controlled by
  automated test software" with no Cancel; it also cannot create tab groups
  (there is no CDP tab-group domain).
* The extension keeps the human's own profile, cookies and windows, and puts
  every agent tab in an orange **"Haider"** tab group
  (`chrome.tabs.group` + `chrome.tabGroups.update({title:"Haider",
  color:"orange"})`).

`browser/haider-presence-extension/` implements the presence half and is
proven on this Mac (Chrome 153, see evidence). webextract-2 owns the
transport and must:

1. **Install/load** the extension (enterprise policy, Chrome Web Store, or for
   development `Extensions.loadUnpacked` over `--remote-debugging-pipe
   --enable-unsafe-extension-debugging`; branded Chrome ≥137 ignores
   `--load-extension`).
2. **Connect** a Native Messaging host named `ai.diffforge.haider.browser`
   (host manifest → a `haiderd` subcommand webextract-2 adds). Messages:
   * daemon → extension: `{op:"open", url}` (agent tab in the Haider group +
     debugger attach), `{op:"cdp", id, tabId, method, params}` (forwarded to
     `chrome.debugger.sendCommand`), `{op:"release"}` (detach every agent tab).
   * extension → daemon: `{event:"attached", tabId}`,
     `{event:"cdp_result", id, result|error}`,
     `{event:"presence", surface:"browser", state:"shown"|"hidden"}`,
     `{event:"stop", surface:"browser", tabId, reason:"canceled_by_user"}`.
3. **Register the renderer**:
   `cu_presence::global().register_renderer(PresenceSurface::Browser, factory)`
   where the factory returns a `PresenceRenderer` over the native-messaging
   port. `send(Hide{..})` must issue `{op:"release"}`; `send(Show|Pointer)` may
   be no-ops (Chrome's bar is the indicator). Feed `{event:"stop"}` into the
   `EventSink` as `PresenceEvent::Stop`.
4. **Hook the dispatcher**: call `PresenceLease::begin(Browser, mark, None, in_flight_cancel)`
   (add a `begin_browser` twin of `begin_computer`) around every browser
   action, honour `PresenceRefusal::Stopped` by journaling `Cancelled`, and
   rely on the existing `close_effects` → `PresenceLease::end`.
5. **Android WebView renderer**: an in-app WebView has no Chrome chrome; show
   the same Android chip ("Haider is controlling this browser") via
   `CuPresence` with the Browser label and keep the WebView inside the app's
   own window (so MediaProjection exclusion is unnecessary — the model reads
   DOM, not pixels).

A CDP-only fallback (headless or a browser without extension support) must
still render presence through the desktop overlay (`Screen` surface), never
silently.

## 6. TUI

While the current run has used `computer`/`mobile` within the idle window the
session header's top-right shows `[ ◉ controlling screen · esc stop ]` (or
`phone`), left of the voice chip. It clears at run end and after 30 s idle.
ESC is the existing turn cancel. The pre-existing transient "◉ controlling
your screen — esc to stop" banner (in-flight control actions only) is
unchanged.

## 7. Known limits

* Windows overlay is compile/Clippy-checked by cross-compilation only; it has
  not run on Windows here.
* Linux has no pointer overlay (see §3); the notification popup can appear in
  screenshots while visible.
* Multi-display: the desktop backends capture the main display; the badge is
  placed on the main screen.
* Android: controller behaviour is proven by JVM tests and the overlay code
  compiles into the APK, but it was not exercised on an emulator. The
  standalone APK has no `mobile.sock` capability client yet (only the legacy
  Termux transport reaches `AndroidCapabilityHandler`), so a daemon-driven
  on-device run is not possible in this tree.
* The macOS "human view" screenshots were taken with the evidence-only
  capturable switch; in production the overlay is invisible to all capture.
