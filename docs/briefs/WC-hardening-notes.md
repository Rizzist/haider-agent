# W-C hardening notes

Fix-first pass over the `wc-codex-review-findings.md` worklist (independent
gpt-5.6-sol xhigh review, coordinator-verified). Every HIGH (H1–H4), every
MEDIUM (M1–M10), and LOW L1 fixed on branch `wc-commands-notify-export`, each
with a regression law that FAILS without the fix and passes with it. Test
ledger 2128 → 2143 (+15 named laws). Commits are per-file so an interruption
preserves progress.

## Commits

| Commit | Scope |
|---|---|
| `a9a4108` | export.rs — H1 opencode time cols + real-schema test, H4 codex `source=cli`+provenance, M1–M4 |
| `6dbcc65` | wc_export_tests.rs — M2/M3/M4 regression laws |
| `576957e` | actor.rs — H2 budget 401 refresh, L1 retry-backoff wakes on Stop |
| `fba600f` | notify.rs — H3 secret masking |
| `473ed2f` | custom_commands.rs — M5–M8 |
| `69d7eb2` | app.rs — M9, M10 |

## Per-finding report

### HIGH

- **H1 — opencode message/part time columns omitted.**
  `export.rs`: the `message` and `part` INSERTs dropped the NOT NULL
  `time_created`/`time_updated` columns, so the first message INSERT fails on a
  real opencode 1.17.x db and the transaction rolls back (feature unusable). Fix:
  carry a per-message `time_created`/`time_updated` (from the turn timestamp) on
  `OpenCodeMessage` and bind them in both INSERT column lists. The TEST schema
  (`wc_export_tests.rs::make_opencode_db`) was corrected to the REAL schema —
  NOT NULL time columns on message AND part — so the law now observes the
  omission (the reduced fixture is what hid the bug).
  Law: `wc_export_tests::opencode_inserts_and_reads_back` (INSERT + populated
  time-column readback).

- **H2 — 401 credential-refresh infinite loop.**
  `haider-core/src/actor.rs::prepare_pre_first_event_retry`: the
  `ProviderAttemptDecision::Retry` arm returned `Ok` unconditionally, bypassing
  the `MAX_API_RETRIES` cap — a resolver that keeps deciding `Retry` on a
  persistently-failing 401 loops forever. Fix: budget the refresh under the same
  cap (`if provider_attempt < MAX_API_RETRIES { refresh; return Ok }`); once
  spent it falls through to the capped-retry / `Errored` path, so a
  non-recovering 401 terminates within a bound. Legitimate refresh-then-succeed
  (a refresh at a low attempt count) is unaffected; the rotation/wait/stop
  arms are untouched.
  Law: `runtime_tests::persistent_401_refresh_terminates_in_errored_within_a_bound`.
  Executed mutation kill (see mutation notes).

- **H3 — notification masking only masked `@`-tokens.**
  `haider-tui/src/notify.rs::mask_text`: masked only whitespace tokens
  containing `@`, so an API key / bearer token / `sk-…` in a session title
  sailed into OSC 9 and the OS notification history. Fix: a `looks_like_secret`
  predicate now routes emails AND secret-shaped tokens (known credential
  prefixes — `sk-`/`pk-`/GitHub PAT/Slack/AWS/Google/`eyJ…` JWT — plus a
  conservative long high-entropy run) through the one P1 masking authority
  (`format::mask_identity`). Ordinary prose is left alone.
  Laws: `wc_notifications_tests::mask_text_hides_api_keys_and_bearer_tokens_not_just_emails`
  and `::notification_osc9_bytes_mask_an_api_key_in_the_title`.
  Executed mutation kill (see mutation notes).

- **H4 — codex export source not listed by `codex resume`.**
  `export.rs::to_codex`: `session_meta` wrote `source:"export"` (not on codex's
  interactive-source allowlist) and copied Haider's origin provider verbatim
  (Anthropic-origin sessions are filtered out). Fix (codex's own suggested
  approach): write an ACCEPTED interactive `source:"cli"` and
  `model_provider:"openai"` (the runtime that will resume the transcript) while
  preserving Haider's true origin in explicit provenance fields
  (`originator:"haider"`, `origin:"haider-export"`, `origin_provider`,
  `origin_model`). Documented in a code comment.
  Law: `wc_export_tests::codex_session_meta_source_is_an_accepted_interactive_value`.

### MEDIUM

- **M1 — orphaned codex `function_call`.**
  Tool turns emitted a `function_call` with possibly-non-JSON `arguments` and NO
  matching `function_call_output`, breaking a resumed turn. Fix: emit a VALID
  paired call + output — `arguments` is a well-formed JSON object
  (`{"summary": …}`) and a `function_call_output` on the same `call_id` closes
  the call. Law: `wc_export_tests::codex_tool_turn_emits_a_paired_call_and_output`.

- **M2 — collision-refusal TOCTOU.**
  `write_new_file` did `path.exists()` then `fs::write`, which followed a
  dangling symlink and wrote through it. Fix:
  `OpenOptions::new().write(true).create_new(true)` — one atomic, symlink-safe
  `O_CREAT|O_EXCL` syscall; `AlreadyExists` is the collision refusal. Law:
  `wc_export_tests::write_new_file_refuses_to_follow_a_symlink`.

- **M3 — rollout + history not recoverable.**
  A failed `history.jsonl` append left the rollout on disk, blocking retry with a
  false collision. Fix: `write_codex_pair` removes the just-created rollout when
  the history append fails (it only removes a file `write_new_file` created, so a
  pre-existing foreign rollout is never touched). Law:
  `wc_export_tests::codex_pair_rolls_back_the_rollout_when_history_fails`.

- **M4 — unbounded replay buffer.**
  The export collected the whole replay into an unbounded `Vec`; a huge/hostile
  session OOMs the exporter. Fix: `collect_bounded_replay` keeps draining the
  (client-API-mandated unbounded) channel but retains at most
  `MAX_REPLAY_EVENTS`, surfacing a truncation notice. Law:
  `wc_export_tests::replay_buffer_is_bounded`.

- **M5 — malformed frontmatter silently accepted.**
  A non-blank, non `key: value` line inside a CLOSED frontmatter was silently
  ignored; the brief required skip-with-warning. Fix: a `MalformedFrontmatter`
  ParseError, DEFERRED so an UNTERMINATED fence still reports as unterminated
  (the pre-existing law is preserved). Laws:
  `wc_custom_commands_tests::malformed_frontmatter_line_is_rejected_not_silently_accepted`
  and `::a_malformed_frontmatter_file_is_skipped_with_a_warning`.

- **M6 — CRLF body offset.**
  `str::lines()` strips the 2-byte CRLF, so `line.len() + 1` undercounted each
  `\r\n` and bled part of the closing `---` fence into the prompt. Fix: walk
  lines WITH their terminators (`split_inclusive('\n')`) for byte-exact offsets.
  Law: `wc_custom_commands_tests::crlf_frontmatter_does_not_bleed_the_closing_fence_into_the_body`.

- **M7 — `.haider/commands` symlink escape.**
  Project discovery followed a `.haider/commands` root symlink with no
  containment check; an untrusted checkout could redirect loading outside the
  repo. Fix: `path_is_contained` canonicalizes the candidate and requires it to
  stay under the ancestor it was found in (fail-closed on a canonicalize error).
  Law: `wc_custom_commands_tests::project_discovery_refuses_a_commands_symlink_escaping_the_repo`.

- **M8 — unbounded walk + no file-size cap.**
  The file limit was applied AFTER collecting every entry, and files were read
  with no size cap. Fix: bound the walk DURING traversal (entries examined +
  files collected caps) and skip a file over the 1 MiB per-file cap with a
  warning. Law: `wc_custom_commands_tests::a_giant_command_file_is_skipped_with_a_warning`.

- **M9 — launcher model-override lost.**
  A launcher custom command with `model:` returned only a note; the next
  `CreateSession` minted the session on the OLD pair. Fix:
  `apply_custom_command_model`'s no-live-session branch now sets the identity
  pair (`identity.provider`/`identity.model_short`, pinned) that `CreateSession`
  reads — exactly like the `/model` picker's launcher branch — so the first turn
  uses the override. Law:
  `wc_custom_commands_tests::launcher_custom_command_model_override_precedes_create_session`.

- **M10 — background/parked terminal transition never notified.**
  The desktop-notification edge was evaluated only in the ACTIVE-session reducer
  (`handle_envelope`); a backgrounded turn reaching Done/Errored notified never.
  Fix: `route_raw` (the real runtime's single event-stream entry point) now
  evaluates a per-session edge (`background_notification_states`) for every
  NON-active session, respecting the same trigger set, toggle, and focus gate.
  Law: `wc_notifications_tests::background_session_terminal_fires_a_desktop_notification`
  (direct `route_raw` — the DemoDriver's `consume_background` bypasses route_raw,
  so a driver-based law would not exercise the fix).

### LOW

- **L1 — retry backoff ignored the actor Stop channel.**
  `wait_before_provider_retry` selected on the turn cancel token and the sleep
  only, so a Stop during a long Retry-After blocked shutdown for the full delay.
  Fix: pin the sleep on a cloned sleeper Arc and loop-select, also watching
  `self.commands.recv()` — a Stop (or a closed channel) returns `Cancelled`
  immediately; other commands are serviced and the SAME deadline is re-awaited.
  Covered by the pre-existing
  `runtime_tests::cancellation_wins_provider_retry_backoff_without_second_request`
  (stays green).

## Verification

- Per-crate suites green: `haider-cli` (incl. 18 export laws), `haider-core`
  (`-- --test-threads=4`, 37 runtime laws incl. H2), `haider-tui` (896 tests, no
  failures). `cargo fmt --all -- --check` exit 0 at every commit; no conflict
  markers. Pre-existing W-C M1–M4 laws and G3/session-map/router regressions
  stay green.
