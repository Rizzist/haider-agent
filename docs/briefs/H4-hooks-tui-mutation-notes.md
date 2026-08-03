# H4 hooks-TUI mutation notes

Every law lives in `crates/haider-tui/tests/h4_hooks_tui_tests.rs` — a
separate test source, never inline. Every mutation below was EXECUTED
against the finished implementation and produced the recorded RUNTIME
failure (an assertion panic), then reverted. The fixtures pin literals —
64-char digests, cwd `/work/h4`, policy `per_digest`, store bound 48,
render bound 8, wire methods `hooks.list`/`hooks.trust`/`hooks.revoke` —
never the production constants they check, and the trust fixtures are
non-degenerate: a trusted row and an untrusted decision row in one
listing; an edited-then-revoked hook beside a never-trusted one; an
applied decision beside a lost proposal.

| # | Production mutation (executed) | Runtime observer | Recorded RUNTIME failure |
|---|---|---|---|
| 1 | `HooksScreenState::apply_snapshot` drops the daemon's rows (`rows = Some(Vec::new())`). | `hooks_screen_lists_daemon_truth_with_trust_states` | Panic: "the trusted row wears ✓ and its number" — the listing renders `no hooks discovered` although the daemon answered two hooks. |
| 2 | `dispatch_hook_trust` installs the answer locally (`row.trusted = confirm.grant`) before the daemon speaks. | `trust_and_revoke_dispatch_receipted_commands_and_install_nothing_locally` | Panic: "dispatch installed NOTHING locally" — the untrusted row flipped on ⏎ with no receipt. |
| 3 | `HooksScreenState::glyph` forgets the trusted baseline — every untrusted row renders plain ○. | `edited_hook_renders_revoked_state` | Panic: "the edited hook renders revoked-by-edit" — the hook trusted under digest `aa…` and re-listed untrusted under `dd…` loses its ✗. |
| 4 | The `absorb_raw_active` call site records hook facts only for `Admission::Apply` (the display gate). | `firings_render_bounded_newest_first` | Panic: store bound assertion `left: 0, right: 48` — hook facts are `render.ui == false` (Skip), so gating on Apply records nothing. |
| 4b | `HookFactsLog::note_envelope` appends oldest-first (`push_back`) and drops the store bound. | `firings_render_bounded_newest_first` | Panic: store bound assertion `left: 53, right: 48`; the ordering assertions also observe `hk-1` leading instead of `hk-53`. |
| 5 | `enter_hooks` removes the `hooks_v1` feature gate — every live daemon is offered the screen. | `ungated_and_demo_are_honest` | Panic: "the screen never moved" `left: Hooks, right: Session` — an ungated daemon opened a screen it cannot serve, and the stale-daemon note (`needs a newer daemon (running v0.0.42)`) never rendered. |
| 5b | `dispatch_hook_trust` removes the demo refusal — demo confirms dispatch a trust request. | `ungated_and_demo_are_honest` | Panic: "demo trust dispatches NOTHING" — an `AppRequest::HooksTrust` escaped the demo world. |
| 6 | `HookFactsLog::note_envelope` lights the chip on any proposal (`proposed_decision.is_some()` instead of `decision_applied`). | `decision_chip_follows_the_journaled_fact` | Panic: "a lost proposal never lights the chip" — the fixture whose journaled fact carries `proposed_decision: Allow, decision_applied: false` wears the chip. |
| 7 | `LiveDriver::apply`'s `HookTrustChanged` arm installs the trust flip from the receipt and skips the chained listing. | `trust_and_revoke_dispatch_receipted_commands_and_install_nothing_locally` | Panic: "the receipt chains daemon truth" `left: [], right: [HooksList { cwd: "/work/h4" }]` — and the receipt-installs-nothing assertion stands behind it. |
| 8 | `link::request_body` encodes every trust change as `hooks.trust` (revoke loses its method). | `trust_and_revoke_dispatch_receipted_commands_and_install_nothing_locally` | Panic: wire-method assertion `left: "hooks.trust", right: "hooks.revoke"`. |

Coverage notes beyond the executed set:

* Law 1 also pins the honest in-flight state ("fetching the daemon's hook
  discovery…" with zero fabricated rows), the captured-at-issuance cwd on
  `AppRequest::HooksRefresh`, the `hooks.list` wire body, and the
  value-carrying `Hit::HookRow(digest)` hit.
* Law 2 additionally pins the session-scoped esc law (esc closes the CARD
  and the screen survives), the durable outbox entry (`outbox_len` 1 → 0
  across the receipt), the receipt's exact `command_id` bytes on the wire,
  the one-at-a-time pending gate, and that only the daemon's NEXT listing
  moves the trust column.
* Law 5's demo half seeds a row through fixture state (fixtures construct
  states; the demo listing is honestly empty) so the refusal is exercised
  against a real confirmation card, not an empty-list no-op.
* The decision chip renders through the real status bar (`hook·decided`
  present after the applied fact, absent after the next run's envelope) —
  journaled fact → chip, never display state.
