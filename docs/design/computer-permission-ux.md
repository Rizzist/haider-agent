# Computer permission UX seam

This backend surface keeps Haider authorization and operating-system
authorization separate. It does not change the global default-deny policy.

## Explicit computer-use consent

A root user message beginning with `computer-use` (an optional leading `/` is
accepted) records explicit intent for that session. The daemon reconstructs
class-scoped `allow_screen` and `allow_screen_control` session grants from the
durable user message, so later computer actions in the same session do not ask
again. A deny policy still wins. Prose which merely discusses computer use,
and child-agent messages, do not opt in.

Set `HAIDER_EXPLICIT_COMPUTER_AUTO_GRANT=0` (also `false`, `no`, or `off`) to
disable this behavior and retain the ordinary per-effect Ask path.

## Grant-needed event

Permission-aware clients opt into the additive raw event family in
`haider_protocol::permission::PermissionEventPayload`. It is intentionally
outside the frozen `EventPayload` enum so older clients may ignore it.

`permission_grant_needed` carries:

- `request_id`: stable card correlation; currently equal to `menu_id`.
- `menu_id`, `request_seq`, `opening_generation`: exact durable menu-CAS
  coordinates for Retry.
- `call_id`, `effect_id`: the parked tool/effect correlation.
- `permission`: `screen_recording` or `accessibility`.
- `pane_name`, `settings_url`: display name and exact
  `x-apple.systempreferences:` deep link.
- `actions`: server-enumerated `open_settings`, `retry`, and
  `restart_daemon` controls.
- `auto_restart_pending`: true once Screen Recording has flipped but the
  current process must be replaced.
- `poll_timeout_ms`: the bounded automatic poll window.

The corresponding `permission_grant_resolved` carries `request_id`,
`permission`, `resolution`, and `retrying_parked_action`.

The event is durable, UI-visible, and omitted from model prompt history. Its
paired ordinary `menu_opened` has `kind=permission`,
`origin=computer-os-permission`, is blocking/session-scoped, and contains one
`retry` / `allow_once` option. This menu is the restart-safe checkpoint; the
rich raw event is the card presentation/action contract.

## Button calls

- **Open Settings:** call
  `haider_client::open_permission_settings(client, session_id, request_id,
  permission)`. The wire request method is
  `computer.permission_open_settings`. It accepts the permission enum, not a
  URL. The daemon requires Control plus a control attachment, proves that the
  matching durable request is unresolved, then invokes `/usr/bin/open` with
  the compiled deep link as one argument. The feature bit is
  `computer_permission_actions_v1`.
- **Retry:** send the existing `WireFrame::MenuAnswer` using `menu_id`,
  `request_seq`, `opening_generation`, option key `retry`, and index `0`.
  Automatic polling uses the same committed menu CAS with `via=hook`; there is
  no second authorization channel.
- **Restart:** call
  `haider_client::restart_daemon_for_permission(profile, options)`. The helper
  opens a fresh same-UID authenticated connection, signals its authenticated
  peer PID exactly once, verifies the `ServerDraining` instance/generation,
  waits for disconnect, starts/attaches through the ordinary daemon ensure
  path, and rejects reattachment to the old generation. Leave the menu
  unanswered: fresh-daemon checkpoint recovery rechecks TCC, resolves Retry,
  and resumes the exact parked tool call.

## macOS state machine

After Haider authorization but before `EffectPhase::Dispatched`, the macOS
backend proactively calls `CGRequestScreenCaptureAccess()` or
`AXIsProcessTrustedWithOptions(...prompt=true)`. If access is absent, the
daemon commits the checkpoint/card and polls the non-prompting preflight.
Accessibility can resume in-process. A Screen Recording false-to-true flip is
conservatively marked restart-required; after the one-click restart, fresh
process preflight resolves the existing checkpoint and the action continues.
No computer effect is recorded as dispatched while it is parked at TCC.

The TCC FFI, native prompt, System Settings opening, and restart-required
Screen Recording decision are macOS-only. The protocol/event/client shapes are
cross-platform-safe; Linux and Windows computer consent behavior is unchanged.
