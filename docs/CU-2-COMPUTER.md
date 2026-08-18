# CU-2 computer backend contract

The provider tool name is `computer`. Its input is a top-level tagged action:

```json
{"action":"screenshot"}
{"action":"cursor_position"}
{"action":"left_click","x":120,"y":240}
{"action":"right_click"}
{"action":"middle_click"}
{"action":"double_click"}
{"action":"left_mouse_down"}
{"action":"left_mouse_up"}
{"action":"mouse_move","x":120,"y":240}
{"action":"left_click_drag","from":{"x":10,"y":20},"to":{"x":300,"y":400}}
{"action":"type","text":"hello"}
{"action":"key","keys":"cmd+shift+4"}
{"action":"scroll","x":640,"y":360,"direction":"down","amount":3}
{"action":"wait","ms":250}
```

`screenshot` and `cursor_position` use `EffectClass::ScreenObserve` (`screen_observe` on the wire). Every other action, including `wait`, uses `EffectClass::ScreenControl` (`screen_control`). Both registry defaults are fail-closed `Ask`: no action dispatches until an existing permission menu is approved. (`Deny` is an irrevocable policy refusal in this broker and therefore cannot mint a session grant.) The menu-minted session grants are named `allow_screen` and `allow_screen_control`; the latter implies observation, while observation never implies control. `allow_exec` implies neither.

`computer` is root-only by default. It and both screen effects are excluded from `default_child_grant`; a delegated child needs an explicit tool/effect ceiling.

No dedicated computer RPC or event was added. The TUI uses the existing tool-call item, permission menu/answer, effect phases, tool result, and run-state events. Hard ESC sends the existing `turn.cancel` request for the active run. Dispatcher close observes that same core cancellation token, flips the backend token, releases retained input state, and records that in-flight dispatch as `EffectOutcome::Cancelled`. An abandoned action without a real turn cancellation remains honestly `Unknown`.

Screenshots are encoded as PNG, passed through the configured redaction policy, admitted through CU-1 `put_image`, and returned as `BoundedResult.images: Vec<ImageBlockRef>`. The default policy is a byte-identical passthrough. Setting `HAIDER_COMPUTER_REDACT_REGIONS` to semicolon-separated `x,y,width,height` rectangles (for example `0,0,640,80;1200,0,400,900`) blackouts those source-image regions before admission. Invalid policy configuration fails closed. Because the hook precedes `put_image`, provider/model context and convergence-graph evidence can only reference the same redacted CAS bytes.

During an active convergence graph, a successfully completed `screenshot` or `inspect` action records supplemental `EvidenceRecorded` through the existing graph journal path. The daemon validates the real `ScreenObserve` Intent → Authorized → Dispatched → `Outcome::Ok` lifecycle, the admitted `ImageBlockRef`, and the graph/node/attempt snapshot before stamping `DaemonVerified` plus the `workspace_revision` at the effect outcome. This evidence is visible provenance and deliberately does not satisfy or fail a graph gate. The model-facing `graph_evidence` tool has no input capable of constructing this source.

Coordinates always refer to the exact image dimensions delivered after CU-1 downscaling. On macOS the backend retains `CGDisplayBounds` in Quartz points and maps a delivered pixel `(x, y)` as:

```text
quartz_x = display_origin_x + x * display_width_points  / delivered_image_width
quartz_y = display_origin_y + y * display_height_points / delivered_image_height
```

This covers both Retina backing scale and the CU-1 2,048-pixel/5-MiB admission bounds. A successful screenshot is required before `cursor_position` or any action carrying screenshot coordinates. A cursor on a different display returns a typed error instead of a false clamped coordinate.

On macOS, CoreGraphics capture/events are guarded by TCC preflights for Screen Recording and Accessibility; failures name the exact System Settings pane and never silently return an empty image. Backend viewport/button state is dispatcher-local, while a process-wide input gate prevents sessions from interleaving Quartz actions or stealing a held left button. Screenshot capture/PNG and redaction work run off the async runtime, and typed text yields between Unicode-scalar-safe batches so cancellation remains responsive.

On Linux, `WAYLAND_DISPLAY` or `XDG_SESSION_TYPE=wayland` positively selects Wayland and never falls through to X11. The shipped `haider-wayland-portal` companion owns one xdg-desktop-portal ScreenCast + RemoteDesktop session, reads the PipeWire capture stream through GStreamer, and sends input through the RemoteDesktop `Notify*` API. `HAIDER_WAYLAND_PORTAL_HELPER` can select another compatible bridge. Requests and responses use the bounded, length-prefixed `haider-cu-wayland-v1` JSON protocol; consent is bounded to 60 seconds, ordinary calls to 15 seconds, cancellation kills the bridge, and unavailable/denied portals return an actionable typed error explaining interactive consent. The release bundle places the companion beside `haiderd`; the logged-in desktop must provide xdg-desktop-portal, PipeWire, `gst-launch-1.0`, and the GStreamer PipeWire/base/good plugins. The portals do not expose a trustworthy global cursor query or accessibility tree, so `cursor_position` and `inspect` return typed unsupported/unavailable errors on Wayland; screenshots embed the cursor for visual grounding.

Outside a Wayland session Linux retains the X11/XTEST implementation. The automated `computer-x11-e2e` workflow explicitly clears `WAYLAND_DISPLAY`, selects `XDG_SESSION_TYPE=x11`, and runs the existing real-pixel/input test under Xvfb. Portals cannot be validated in Xvfb. Manual Wayland validation requires a real logged-in session and explicit consent:

```sh
  cargo build -p haider-tools --bin haider-wayland-portal --locked
HAIDER_CU_WAYLAND_E2E=1 \
  HAIDER_WAYLAND_PORTAL_HELPER="$PWD/target/debug/haider-wayland-portal" \
  cargo test -p haider-tools --test linux_wayland_computer_e2e --locked -- \
  --ignored --test-threads=1
```

On Windows, the named-pipe transport authenticates the connecting process before protocol framing: `GetNamedPipeClientProcessId`/`GetNamedPipeServerProcessId` identifies the peer, `OpenProcess` + `OpenProcessToken` retrieves `TokenUser`, and `EqualSid` compares it with the daemon token. Every lookup failure is fail-closed. Same-process and SID-comparison pins run on Windows; rejecting a genuinely different logged-in user remains a manual multi-user Windows validation.
