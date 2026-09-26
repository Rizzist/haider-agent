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

`screenshot` and `cursor_position` use `EffectClass::ScreenObserve` (`screen_observe` on the wire). Every other action, including `wait`, uses `EffectClass::ScreenControl` (`screen_control`). Both registry defaults are `Ask`: interactive sessions open the existing permission menu, while autonomous sessions resolve Ask to ordinary journaled `Allow`. (`Deny` is an irrevocable explicit policy refusal in this broker and therefore cannot mint a session grant.) The menu-minted interactive session grants are named `allow_screen` and `allow_screen_control`; the latter implies observation, while observation never implies control. `allow_exec` implies neither. Operating-system Screen Recording or Accessibility authorization remains a separate external prerequisite and is never fabricated by autonomous mode.

`computer` is root-only by default. It and both screen effects are excluded from `default_child_grant`; a delegated child needs an explicit tool/effect ceiling.

No dedicated computer RPC or event was added. The TUI uses the existing tool-call item, permission menu/answer, effect phases, tool result, and run-state events. Hard ESC sends the existing `turn.cancel` request for the active run. Dispatcher close observes that same core cancellation token, flips the backend token, releases retained input state, and records that in-flight dispatch as `EffectOutcome::Cancelled`. An abandoned action without a real turn cancellation remains honestly `Unknown`.

Screenshots are encoded as PNG, passed through the configured redaction policy, admitted through CU-1 `put_image`, and returned as `BoundedResult.images: Vec<ImageBlockRef>`. The default policy is a byte-identical passthrough. Setting `HAIDER_COMPUTER_REDACT_REGIONS` to semicolon-separated `x,y,width,height` rectangles (for example `0,0,640,80;1200,0,400,900`) blackouts those source-image regions before admission. Invalid policy configuration fails closed. Because the hook precedes `put_image`, provider/model context and convergence-graph evidence can only reference the same redacted CAS bytes.

During an active convergence graph, a successfully completed `screenshot` or `inspect` action records supplemental `EvidenceRecorded` through the existing graph journal path. The daemon validates the real `ScreenObserve` Intent → Authorized → Dispatched → `Outcome::Ok` lifecycle, the admitted `ImageBlockRef`, and the graph/node/attempt snapshot before stamping `DaemonVerified` plus the `workspace_revision` at the effect outcome. This evidence is visible provenance and deliberately does not satisfy or fail a graph gate. The model-facing `graph_evidence` tool has no input capable of constructing this source.

Coordinates always refer to the exact image dimensions delivered after CU-1 downscaling. On macOS the backend retains `CGDisplayBounds` in Quartz points and maps a delivered pixel `(x, y)` as:

```text
quartz_x = display_origin_x + x * display_width_points  / delivered_image_width
quartz_y = display_origin_y + y * display_height_points / delivered_image_height
```

This covers both Retina backing scale and the computer-screenshot bounds. After redaction (and any region crop), every computer screenshot is downscaled by ONE uniform factor so that its long edge is at most 1,568 px and its area at most 1.15 MP (`COMPUTER_SCREENSHOT_MAX_DIMENSION`, `COMPUTER_SCREENSHOT_MAX_PIXELS` in `haider-protocol`), each axis floored; the general CU-1 2,048-pixel/5-MiB admission bounds still apply to every other tool image. Only the bounded image is admitted to CAS, and its dimensions become the delivered viewport that all later coordinates refer to (region requests carry `reference_width`/`reference_height` and stay scale-invariant). Android has no PNG decoder in its build and returns a typed `Unavailable` for this path instead of guessing. A successful screenshot is required before `cursor_position` or any action carrying screenshot coordinates. A cursor on a different display returns a typed error instead of a false clamped coordinate.

### Screenshot history in provider requests

Durable history keeps every screenshot's `ImageBlockRef`; only the provider-bound clone of each request is projected (`apply_tool_result_image_budget`):

- **Stale-screenshot elision.** Only the newest `computer` tool result keeps its image. Every older computer screenshot is removed from that request and its result text gains one `haider_elision_v1` marker line with `"scope": "computer_screenshot_history"`, the omitted byte/image counts, and the first omitted artifact (the full image stays retrievable from CAS). Afterwards the existing oldest-first turn budget (5 images / 16 MiB, marker scope `tool_result_image_budget`) applies; unsupported-vision providers get artifact-naming placeholders (scope `tool_result_image_capability_degradation`).
- **Typed projection record.** The projection returns a `ToolResultImageProjection` naming every tool result whose images it removed (stale elision and turn budget; capability degradation is excluded because it never rewrites an already-sent prefix). The daemon's prompt compile and each actor request merge their records onto `TurnRequest.tool_result_image_projection`, and compaction carries it onto the summary request. Adapters act ONLY on this typed record; marker text inside tool output, files or fetched pages is never parsed, so it cannot switch provider policy on. The record is recomputed from durable history for every request and only grows, so it survives restart and resume.
- **Anthropic prefix binding (drop policy).** On prefix-binding models (Opus 5.5, Fable 5.1, Mythos 5.1) a non-empty record means an earlier tool result was rewritten, which invalidates signed thinking bound to the old prefix. Those requests carry `thinking.block_binding.prefix_mismatch_behavior = "drop_block"` plus the `thinking-binding-controls-2026-08-01` beta (joined into the single `anthropic-beta` header for every auth mode). Other models and append-only conversations keep their exact prior wire. No prefix-binding model maps to the native Anthropic computer tool (a guard test pins this), so its display-size change cannot rewrite the tools prefix.
- **Binding fallback.** If a route (OAuth, Vertex, a custom gateway, …) answers 400 naming `block_binding`, `prefix_mismatch_behavior` or the beta, the adapter resends the same prepared request ONCE without the policy, latches that route (auth mode, URL, model) as unsupported for the daemon process and logs a warning; later requests on the route render the pre-policy wire directly, so the fallback cannot loop. Any other 400 is returned unchanged. The latch is process-lifetime: after a daemon restart the first policy request on such a route costs one more rejected attempt.
- **OpenAI native computer calls.** The Responses contract needs a `computer_screenshot` on every replayed `computer_call_output`. An output whose screenshot the typed record marks as elided replays a fixed 16x16 grey PNG placeholder; the newest keeps the real capture. A screenshot missing without that record is still rejected.

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

While any `computer`/`mobile` action runs, the human sees a presence indicator ("Haider is controlling this screen · Stop", an agent pointer, a phone chip, or Chrome's debugging bar) whose Stop cancels the in-flight action and the run through the same receipt-backed turn cancellation as ESC. See `docs/cu-presence.md`.
