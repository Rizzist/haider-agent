# W-C hardening mutation notes

Kills for the `wc-codex-review-findings` hardening pass, branch
`wc-commands-notify-export`, 2026-08-10. H2 and H3 were EXECUTED end-to-end
(commit-before-mutation → single anchor → the ONE named test in isolation
["running 1 test"] → observed RUNTIME failure, never a compile error →
`git checkout --` revert → green). H1 and H4 landed in the coordinator-committed
export commit (`a9a4108`) before this session could run their kills, so they are
documented from their laws. The remaining MEDIUM/LOW kills are the reasoned
production-mutation → observing-law pairs.

## EXECUTED kills

### H2 — 401 credential-refresh budget

- Commit before mutation: `576957e` (tree clean).
- Anchor: `crates/haider-core/src/actor.rs`, the single Retry-arm gate
  `if provider_attempt < MAX_API_RETRIES {` → `if provider_attempt < MAX_API_RETRIES || true {`
  (defeats the budget; the refresh becomes unconditional — the original bug).
- Test in isolation: `cargo test -p haider-core --test runtime_tests persistent_401 -- --test-threads=4` → `running 1 test`.
- Observed RUNTIME failure:
  `test persistent_401_refresh_terminates_in_errored_within_a_bound ... FAILED`,
  panic at `runtime_tests.rs:1871`: **"the refresh must be budgeted under
  MAX_API_RETRIES (consulted 51 times)"** — the unbudgeted resolver is consulted
  until the test's 50-call safety hatch fires (51st `fetch_add`), proving the
  loop no longer terminates within the cap.
- Revert `git checkout -- crates/haider-core/src/actor.rs`; re-ran the test →
  `ok. 1 passed`.

### H3 — notification secret masking

- Commit before mutation: `fba600f` (tree clean).
- Anchor: `crates/haider-tui/src/notify.rs::looks_like_secret` — inserted
  `return false;` immediately after the `@`-token check (reverting to the old
  `@`-only behaviour; the prefix/entropy rules become dead).
- Test file in isolation: `cargo test -p haider-tui --test wc_notifications_tests`.
- Observed RUNTIME failures (2), email path still green:
  - `mask_text_hides_api_keys_and_bearer_tokens_not_just_emails ... FAILED` —
    **"sk- key leaked: deploy sk-ant-api03-SEKRET1234567890abcd for prod"**.
  - `notification_osc9_bytes_mask_an_api_key_in_the_title ... FAILED` —
    **"raw key leaked into the line: haider: turn done — release
    sk-ant-api03-DEADBEEFsecret0001x"**.
  - `masked_text_hides_an_email_via_the_one_authority ... ok` (the `@` path is
    unchanged), confirming the kill is specific to the secret extension.
- Revert `git checkout -- crates/haider-tui/src/notify.rs`; re-ran the file →
  `ok. 14 passed`.

## Documented from their laws (committed in `a9a4108` before this session)

### H1 — opencode message/part time columns

| Production mutation | Observing law | Expected RUNTIME failure |
|---|---|---|
| Drop `time_created`/`time_updated` from the `message` (or `part`) INSERT column list + bindings. | `wc_export_tests::opencode_inserts_and_reads_back` | The INSERT hits the NOT NULL constraint on the corrected real-schema fixture — `write_opencode(...).expect("insert")` panics (transaction rolls back), exactly the production symptom on opencode 1.17.x. |
| Keep the reduced fixture schema (no time columns on message/part). | same | The omission is no longer observable — this is the degenerate-fixture trap the fix explicitly corrects; the schema now mirrors the real store. |

### H4 — codex export source

| Production mutation | Observing law | Expected RUNTIME failure |
|---|---|---|
| Restore `source:"export"` (or copy the origin provider into `model_provider`). | `wc_export_tests::codex_session_meta_source_is_an_accepted_interactive_value` | `assert ACCEPTED.contains(&source)` fails — `"export"` is not in `{cli, vscode, exec}` — and/or `model_provider != "openai"`; the provenance assertions still pin the true origin. |

## Reasoned MEDIUM / LOW kills

| # | Production mutation | Observing law | Expected RUNTIME failure |
|---|---|---|---|
| M1 | Drop the `function_call_output` emit (or make `arguments` the bare summary string). | `wc_export_tests::codex_tool_turn_emits_a_paired_call_and_output` | No output with the call's `call_id` (unpaired) or `arguments` fails to parse as JSON. |
| M2 | Restore `if path.exists() { … } fs::write`. | `wc_export_tests::write_new_file_refuses_to_follow_a_symlink` | The dangling symlink is followed — a target file is created and `Ok` returned instead of `Collision`. |
| M3 | Remove the `remove_file(rollout_path)` rollback on history failure. | `wc_export_tests::codex_pair_rolls_back_the_rollout_when_history_fails` | The rollout survives the failed history append (`rollout.exists()`), blocking a retry with a false collision. |
| M4 | Retain every item (`if events.len() >= max_events` never truncates). | `wc_export_tests::replay_buffer_is_bounded` | `events` holds all 10 items and `truncated == false` — the buffer is unbounded again. |
| M5 | `continue` on a non `key: value` line instead of flagging malformed. | `wc_custom_commands_tests::malformed_frontmatter_line_is_rejected_not_silently_accepted` / `::a_malformed_frontmatter_file_is_skipped_with_a_warning` | The malformed file parses `Ok` (silent accept) — no `MalformedFrontmatter`, no skip, no warning. |
| M6 | Revert the offset to `line.len() + 1` (or `str::lines()`). | `wc_custom_commands_tests::crlf_frontmatter_does_not_bleed_the_closing_fence_into_the_body` | The body is `"-\r\nShip it now…"` — the closing-fence dash bleeds in; `body.trim() != "Ship it now"` and `body.starts_with('-')`. |
| M7 | Drop the `path_is_contained` check (return `Some(candidate)`). | `wc_custom_commands_tests::project_discovery_refuses_a_commands_symlink_escaping_the_repo` | Discovery returns the escaping symlink and `load_for` loads the external `leak` command. |
| M8 | Apply the file cap only after collecting all entries / read without the size check. | `wc_custom_commands_tests::a_giant_command_file_is_skipped_with_a_warning` | The over-cap `huge` command loads and no warning names it. |
| M9 | Restore the note-only launcher branch (no identity set). | `wc_custom_commands_tests::launcher_custom_command_model_override_precedes_create_session` | `identity.provider`/`model_short` stay at the default (`anthropic`/`fable-5`); the override never reaches `CreateSession`. |
| M10 | Remove the `route_raw` background-notification evaluation. | `wc_notifications_tests::background_session_terminal_fires_a_desktop_notification` | The background session's terminal transition queues no notification (`notifications` empty). |
| L1 | Drop the `self.commands.recv()` arm from the retry-backoff select. | `runtime_tests::cancellation_wins_provider_retry_backoff_without_second_request` (stays green as coverage) | A Stop during a long Retry-After blocks shutdown for the full delay instead of returning `Cancelled` promptly. |

## Review of record (coordinator, Fable, executed post-lane)

The lane executed the H2 and H3 kills; it could not execute H1/H4 (their
code landed in the coordinator's earlier export checkpoint a9a4108). I
executed both here to close the HIGH set:

- **H1** (opencode time cols): reverted the message INSERT to the pre-fix
  column list (dropped time_created/time_updated). `wc_export_tests`
  opencode laws FAILED with `NOT NULL constraint failed:
  message.time_created` — proving the CORRECTED test schema now observes
  the omission the old reduced fixture hid (the degenerate-fixture class).
  Reverted; green.
- **H4** (codex source): reverted `source:"cli"` → `"export"`.
  `codex_session_meta_source_is_an_accepted_interactive_value` FAILED
  ("session_meta source must be codex-accepted, got \"export\"").
  Reverted; green.

All four HIGH fixes are now executed-kill-verified (H2/H3 by the lane,
H1/H4 by the coordinator). Spot-checked the lane's H2 kill note (panic
"refresh must be budgeted … consulted 51 times") and H3 (sk-/raw-key
leaks) against the notes — consistent. The M10 driver-vs-route_raw note
is a correct observation (the DemoDriver bypasses route_raw). Campaign
ACCEPTED.
