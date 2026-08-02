# B4b attachments-TUI mutation notes

Every mutation below was EXECUTED against the suite on 2026-08-02 (apply →
observe the runtime failure → revert). "Expected RUNTIME failure" means the
named test fails by assertion; a compile-only failure is not the claimed
evidence. All observers live in `haider-tui/tests/b4b_attach_tests.rs`.
Observer assertions use literals (exact base64 strings, exact flash text,
exact JSON block shapes) — never the production constants they pin.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| `attach_command` drops the `daemon_serves(FEATURE_ARTIFACT_PUT_V1)` gate (`if false`). | `feature_ungated_attach_is_honest` | The ungated daemon gets a live `AttachRead` instead of the honest notice: the flash equality on `"· attachments needs a newer daemon (running v0.0.50) — restart it to pick up this release"` fails with `left: None`. |
| `attach_command` drops the `fabricates_locally` demo gate. | `demo_attach_refuses` | Demo `daemon_serves` answers "capable" for everything, so `/attach` sails past the feature gate and pushes a read the demo world can never honor — the `"· /attach — live only; attachments ride the daemon's store"` flash equality fails with `left: None`. |
| `request_body`'s `ArtifactPut` arm encodes `STANDARD_NO_PAD` instead of RFC 4648 `STANDARD`. | `attach_uploads_then_submit_carries_refs` | The exact-wire equality fails: `data_base64: "iVBORw0KGgpoYWlkZXItYjRiLXRlc3QtaW1hZ2U"` (no `=`) against the literal padded expectation. |
| `request_body`'s `Submit` arms encode `attachments: vec![]` in both `turn.submit` forms. | `attach_uploads_then_submit_carries_refs`, `pending_attachments_clear_on_submit_and_survive_branch_switch_capture` | Both wire pins fail with `attachments: Array []` against the literal image-block JSON — legacy AND branch-capable forms (2 failures observed). |
| `big_paste`'s live gated branch regresses to the sim's theater (insert the `[Pasted N lines]` token, drop the content, upload nothing). | `paste_pill_uploads_pasted_text_and_rides_submit` | The composer-text equality fails: `text: "[Pasted 5 lines] "` where the REAL pill leaves `""` and a chip; no `ArtifactPut` is found. |
| `Composer::take_ready_attachments` reads without `std::mem::take` (the chips survive the submit). | `pending_attachments_clear_on_submit_and_survive_branch_switch_capture`, `paste_pill_uploads_pasted_text_and_rides_submit` | "pending attachments clear on submit" and "the pill cleared on submit" both fail — the drained blocks ride AND stay, so the next submit would double-send (2 failures observed). |
| `submit_composer` drops the `has_uploading_attachment` gate (`if false`). | `attach_uploads_then_submit_carries_refs` | The mid-upload submit goes out instead of refusing: the `"· attachment still uploading — a moment"` flash equality fails with `left: None` — and the in-flight chip would have been silently shed (`ready_block` filters it). |
| `complete_upload` ignores the captured surface and completes on the DISPLAYED composer. | `upload_reply_completes_the_issuing_draft_not_the_displayed_surface` | The parked issuing draft's chip never turns ready: the `artifact` equality fails with `left: None`. FINDING: this mutation SURVIVED the original six laws — every reply landed while the issuing surface was still on screen — so this observer was added mid-round (park the draft under ⌃C first, then apply the reply) and the failure re-observed before the revert. |
| The ⌫ chip-removal arm drops the `cursor() == 0` guard (any backspace eats a chip). | `pending_attachments_clear_on_submit_and_survive_branch_switch_capture` | Mid-text ⌫ pops the newest chip instead of deleting the grapheme: the `composer == ""` / chips-len-2 pair fails with `text: "x"` and the pasted chip gone. |
| `LiveDriver::apply`'s `Disconnected` arm drops the `drop_uploading_attachments` sweep. | `disconnect_drops_uploading_chips_with_an_honest_notice` | "the in-flight chip died with the socket" fails — the chip spins "uploading" forever for a receipt-free request whose reply can never arrive, and the reconnect flash loses its dropped-upload notice. |

Structural notes (not mutations):

- The ⌫-with-cursor-position and disconnect-sweep observers were written in
  the same hardening pass as the survivor finding above (their mutations
  were predicted survivors by inspection of the six-law set); both
  mutations were then executed and their runtime failures observed as
  tabled.
- The read seam is deliberately un-mutated at the sniff level:
  `attach_read_effects` delegates magic sniffing and the 5 MiB bound to
  `haider_client::load_image_attachment` — the SAME function `haider run
  --attach` uses, already pinned by `haider-client`'s own suite.
  `oversized_and_non_image_attach_are_honest_notices_with_no_upload` pins
  this crate's half: the honest flash wording, no chip, no upload request.
- `ArtifactPut` deliberately returns `None` from `LiveCommand::command_id`
  (receipt-free, never outboxed). A "give it a command id" mutation is
  unrepresentable without inventing wire fields the daemon does not
  decode; the disconnect-sweep law above pins the behavioral consequence
  (nothing resends; the chip dies honestly).
