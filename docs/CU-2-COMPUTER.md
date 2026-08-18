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

Screenshots are encoded as PNG, admitted through CU-1 `put_image`, and returned as `BoundedResult.images: Vec<ImageBlockRef>`. Coordinates always refer to the exact image dimensions delivered after CU-1 downscaling. On macOS the backend retains `CGDisplayBounds` in Quartz points and maps a delivered pixel `(x, y)` as:

```text
quartz_x = display_origin_x + x * display_width_points  / delivered_image_width
quartz_y = display_origin_y + y * display_height_points / delivered_image_height
```

This covers both Retina backing scale and the CU-1 2,048-pixel/5-MiB admission bounds. A successful screenshot is required before `cursor_position` or any action carrying screenshot coordinates. A cursor on a different display returns a typed error instead of a false clamped coordinate.

The real backend is compiled only on macOS. It uses the owner's `snipping.rs` CoreGraphics FFI shapes for capture/events plus TCC preflights for Screen Recording and Accessibility; failures name the exact System Settings pane and never silently return an empty image. Backend viewport/button state is dispatcher-local, while a process-wide input gate prevents sessions from interleaving Quartz actions or stealing a held left button. Screenshot capture/PNG work runs off the async runtime, and typed text yields between Unicode-scalar-safe batches so ESC remains responsive. Other platforms select the typed `UnavailableComputerBackend` stub. Real display/TCC execution is covered only by an ignored manual test; automated tests inject a deterministic fake backend.
