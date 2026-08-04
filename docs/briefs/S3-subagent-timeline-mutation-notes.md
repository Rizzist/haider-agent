# S3 subagent-timeline mutation notes

Each mutation below was applied to production code, its separate observer was
run to an assertion-level RUNTIME failure (`running 1 test` verified on every
run — no vacuous filters), and the mutation was reverted against the committed
tree. No compile-only failure is claimed as evidence. Fixtures are
non-degenerate: the messaged fact carries a real preview and BOTH delivery
kinds, the chip receives TWO agent-scoped user rows (spawn prompt + later
steer), and the composer round-trip crosses the real `request_body` /
`map_response` seams with a receipt naming a live child run.

| # | Production mutation (applied → reverted) | Runtime observer (`s3_subagent_timeline_tests`) | Observed RUNTIME failure |
|---|---|---|---|
| M1 | Swapped the `steer`/`queued` literals in `app::messaged_note`'s delivery match. | `agent_messaged_renders_in_main_timeline_with_delivery` | Tail assertion failed with `→ messaged Ammar (r) · fix the tests first · queued` where `· steer` was law. |
| M2 | Replaced `absorb_raw_active`'s decode-error fallback (`session::route_agent_event`) with the old bare `count_unknown_payload()`. | `agent_messaged_renders_in_main_timeline_with_delivery` | `expect("the fact painted a note")` panicked — the fact vanished into the unknown counter. |
| M3 | Collapsed `session::chip_apply` to a plain `chip.transcript.apply(payload)` (no from-main marking). | `chip_view_shows_steer_messages` | `left: [false, false]` vs `right: [true, true]` — both agent-scoped rows fell back to plain user rows. |
| M4 | Replaced the driver's `AppRequest::ChipSubmit` arm with a silent `Vec::new()` discard. | `chip_composer_rides_the_steer_wire_and_flashes_daemon_receipt` | `expect("one command")` panicked — no `agent.message` was minted. |
| M5 | Swapped the two receipt-flash format strings in `LiveDriver::apply`'s `AgentMessaged` arm. | `chip_composer_rides_the_steer_wire_and_flashes_daemon_receipt` | Flash assertion failed with `· messaged Ammar (r) — queued as a fresh child turn` for a `DeliveredSteer` receipt. |
| M6 | Dropped the reducer's `daemon_serves(FEATURE_AGENT_MESSAGE_V1)` gate (`if false`). | `demo_and_ungated_are_honest` | The ungated half failed at RUNTIME — a `ChipSubmit` request rode a wire the daemon never advertised. |
| M7 | Broke `BranchState::content_scope`'s active-branch arm (`if false`). | `agent_messaged_renders_in_main_timeline_with_delivery` | `expect("the fact painted a note")` panicked — the active-branch fact was mis-scoped and painted nothing. |
| M8 | Emptied the rendered ` · from main` tag span in `render::transcript_lines`. | `chip_view_shows_steer_messages` | Rendered-row assertion failed with ` → focus on fs_edit …` (sigil present, tag gone). |

Adjacent structural pin: `timeline_order_spawned_messaged_report` holds the
strict entry ordering spawned(index) < messaged(index) < report(index) in ONE
transcript, so any mutation that re-routes the marker to a side surface (M2,
M7 class) also fails it.

## Honest flips carried by this wave

* `s2_ui_refinement_tests::child_view_renders_user_messages` — the chip's
  agent-scoped user rows now pin the `→` from-main sigil (they pinned `❯`
  when the rows were indistinguishable from local ones).
* `w3c31_r2_tests::the_driver_refuses_demo_vocabulary_aloud_…` — `ChipSubmit`
  left the demo-only refusal roster (it is live vocabulary now).
* `w3c31_r2_tests::live_subagent_steer_and_close_refuse_…` — the steer half
  now pins the stale-daemon refusal (`messaging a subagent`) for a daemon
  without `agent_message_v1`; the close half keeps its demo-only refusal.

## Wire gap noted honestly (scope item 4 skipped)

The chip header's handoff-dir hint was NOT built: the ephemeral handoff path
(`<workspace>/.haider/handoff/<blake3(parent_session)[..16]>`) is advertised
ONLY inside the child's system prompt (`SystemPromptBuilder::build_with_handoff`,
haider-daemon/src/worker.rs). Neither the `AgentSpawned` manifest (its
`coordinates` carry parent/run/call/tool/child-session ids only) nor any
journal fact exposes it, so a TUI hint would have to re-derive the daemon's
private path scheme — fabrication, refused. Backlog: put the path (or its
short form) on the manifest `coordinates` when the delegation carries one.
