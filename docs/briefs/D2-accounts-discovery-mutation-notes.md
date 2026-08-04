# D2 accounts discovery (TUI) — mutation notes

Every kill below was EXECUTED on 2026-08-05: the production mutation was
applied, the single named observer was run with
`cargo test -p haider-tui --test d2_device_discovery_tests <observer>`
(output showed the one matching test FAILING at runtime with the quoted
panic), then the mutation was reverted and the full crate suite + fmt +
`clippy -p haider-tui --all-targets -- -D warnings` were re-run green. A
compile failure is never the claimed evidence.

Scope note (owner amendment inherited from D1): NO refresh action exists —
re-login/re-import is the freshness path — so the section renders freshness
as a HINT and offers no refresh affordance to mutate. The candidates read
rides screen entry only; K8 pins that no polling path exists to resurrect.

## K1 — freshness hint dropped (EXECUTED, reverted)

- Mutation: `render.rs::push_device_candidates_section` — delete the
  `format!(" · {}", candidate.freshness)` span from supported rows
  (`let _ = freshness_style;`).
- Observer: `device_section_lists_candidates_with_freshness`
- Observed RUNTIME failure: panic at the supported-row assertion —
  `[1] Codex CLI · openai · you@work.com · fresh` absent from the drawn
  `/accounts` frame.

## K2 — import without the outbox (EXECUTED, reverted)

- Mutation: `live.rs::handle_request`, `AppRequest::DeviceImport` arm —
  return the built `LiveCommand::DeviceImport` WITHOUT `self.enqueue(..)`
  (dispatch fires, nothing durable waits).
- Observer: `one_key_import_dispatches_receipted_command_and_installs_nothing_locally`
- Observed RUNTIME failure:
  `assertion 'left == right' failed: receipted + durable: the import waits
  in the outbox — left: 0, right: 1`.

## K3 — supported-flag guard deleted (EXECUTED, reverted)

- Mutation: `app.rs::import_device_candidate` — delete the
  `if !candidate.import_supported { return; }` early return.
- Observer: `unsupported_rows_are_dim_honest_and_inert`
- Observed RUNTIME failure: panic
  `a forged unsupported coordinate is inert: [DeviceImport { candidate:
  "dev-gemini-1" }]` — the forged hit dispatched a real import request.

## K4 — entry gate dropped (EXECUTED, reverted)

- Mutation: `app.rs::enter_accounts` — push
  `AppRequest::DeviceCandidatesRefresh` unconditionally (the
  `device_discovery_available` gate removed).
- Observer: `ungated_daemon_hides_the_section`
- Observed RUNTIME failure: panic
  `an ungated daemon is never asked: [AccountsRefresh,
  DeviceCandidatesRefresh]`.

## K5 — demo-true capability delegate (EXECUTED, reverted)

- Mutation: `app.rs::device_discovery_available` — delegate to
  `self.daemon_serves(FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1)` (which is
  demo-true by design).
- Observer: `demo_is_honest`
- Observed RUNTIME failure: panic
  `demo never asks a daemon it does not have: [AccountsRefresh,
  DeviceCandidatesRefresh]`.

## K6 — wrong wire body for the read (EXECUTED, reverted)

- Mutation: `link.rs::request_body` — map `LiveCommand::DeviceCandidates`
  to `RequestBody::AccountList { provider: None }` (a plausible
  copy-paste).
- Observer: `the_wire_shapes_round_trip`
- Observed RUNTIME failure: panic at the read's request-body `matches!`
  assertion.

## K7 — failure never releases the gate (EXECUTED, reverted)

- Mutation: `live.rs::apply`, the `Failed` arm's `pending_device_import`
  branch — delete `model.device.pending_import = None;`.
- Observer: `a_failed_import_releases_the_gate_with_the_honest_reason`
- Observed RUNTIME failure: panic `the gate releases` — the pending
  candidate survived its own typed failure, wedging every later import.

## K8 — accounts refresh polls discovery (EXECUTED, reverted)

- Mutation: `live.rs::handle_request`, `AppRequest::AccountsRefresh` arm —
  return `vec![AccountList, DeviceCandidates]` (a plausible
  "keep it fresh").
- Observer: `the_candidates_read_rides_screen_entry_only`
- Observed RUNTIME failure: panic
  `account.list truth does not poll discovery: [AccountList,
  DeviceCandidates]`.

## K9 — cursor extension dropped (EXECUTED, reverted)

- Mutation: `app.rs::handle_accounts_key`, `Down` arm — clamp the cursor
  to `self.accounts.rows.len()` alone (the supported-candidate extension
  removed).
- Observer: `enter_on_the_highlighted_candidate_imports_it`
- Observed RUNTIME failure:
  `assertion 'left == right' failed: the flattened selectable rows extend
  into the candidates` — ⏎ could never reach a candidate.

## Wire gaps / recorded decisions

- `discovery_disabled: true` and an empty report both keep the section
  ABSENT. The disabled flag is held in `DeviceCandidatesState` but no copy
  distinguishes the two on screen — the section never claims "nothing
  found", so absence is honest for both. Revisit only if the owner wants a
  visible "discovery is switched off" note.
- After a successful import the candidate row REMAINS listed (its store is
  still on the device) and its freshness hint is whatever screen entry
  reported — re-entering the screen is the only re-read, per the
  entry-only law. The imported account itself appears via the chained
  `account.list`.
- `AccountsState::apply_snapshot` clamps the cursor to the account rows,
  so a snapshot that applies while the cursor sits in the candidate zone
  (e.g. the refresh chained off an import receipt) pulls the highlight
  back to the last account row. Cosmetic; digits and clicks are
  unaffected.
- A reconnect resends the durable import from the outbox but does NOT
  re-read candidates (screen-entry law); a section shown across a redial
  can go stale until the user re-enters the screen.
- The ladder was not run: no shared render geometry moved — the section
  lives inside the `/accounts` footer and `/providers` buttons area, and
  no probe visits either screen.
