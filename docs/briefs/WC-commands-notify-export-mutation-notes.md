# W-C (commands · notify · export · retry) mutation notes

Each mutation below was EXECUTED against the `wc-commands-notify-export`
branch on 2026-08-09: the tree was clean (commit-before-mutation), a single
`python3` anchor asserted the production string occurs exactly once
(`src.count(old) == 1`) before rewriting it, the ONE named test was run in
isolation (`running 1 test`), the RUNTIME failure (assertion/panic — never a
compile error) was observed and recorded, then the file was reverted
(`git checkout --`) and the tree returned clean. Eight kills across the four
milestones (M1/M2/M3 laws were already committed; M4's landed with this
wave). The recording-sleeper seam makes the two M4 kills wall-clock-free.

## M1 — custom slash commands

| # | Production mutation | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| 1 | `custom_commands::substitute` — the `$ARGUMENTS` arm joins with `","` instead of `" "` (`args.join(" ")` → `args.join(",")`). | `wc_custom_commands_tests::substitution_covers_all_three_forms_and_empty_positionals` | `assertion left == right failed` — `left: "all: a,b,c"`, `right: "all: a b c"`: `$ARGUMENTS` is no longer space-joined. |
| 2 | `custom_commands::load` — the two source-load blocks are swapped so the GLOBAL dir loads last and overwrites the project entry on a name collision (project no longer wins). | `wc_custom_commands_tests::project_wins_over_global_on_name_collision` | `assertion left == right failed` — `left: "GLOBAL greeting"`, `right: "PROJECT greeting"`: precedence inverted. |

## M2 — desktop notifications

| # | Production mutation | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| 3 | `app::note_run_state_for_notifications` — the focus gate suppresses on `focus_reported && focused` → `focus_reported \|\| focused`, so the focus-NEVER-reported fallback no longer fires. | `wc_notifications_tests::focus_gate_suppresses_when_focused_but_fires_when_focus_unreported` | `assertion left == right failed: fallback fires` — `left: 0`, `right: 1`: the unreported-focus terminal went silent. |
| 4 | `notify::osc9_for_tty` — the tty guard is inverted (`if is_tty` → `if !is_tty`): bytes are emitted to a pipe and withheld from a tty. | `wc_notifications_tests::non_tty_sink_emits_no_osc_bytes` | `assertion failed: !notify::osc9_for_tty(line, true).is_empty()`: a real tty received no OSC bytes (and a pipe would have leaked them). |

## M3 — session export (+ cross-harness)

| # | Production mutation | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| 5 | `export.rs` codex writer — the `session_meta` `id` is desynced from the filename uuid (`"id": uuid` → `"id": format!("{uuid}-mutant")`). | `wc_export_tests::codex_rollout_id_equals_filename_uuid` | `assertion left == right failed` — `left: "0198…d123-mutant"`, `right: "0198…d123"`: `codex resume` would never find the file. |
| 6 | `export.rs` `write_opencode` — the session-collision guard is inverted (`if exists` → `if !exists`), so a fresh session is refused and a colliding one would overwrite. | `wc_export_tests::opencode_refuses_a_session_collision` | panic `first insert: Collision("opencode session ses_7dcf79c7f6fa627e")`: the guard fired on the FIRST (non-colliding) write instead of the second — the refusal moved to the wrong side. |

## M4 — API-error retry with a visible attempt counter

| # | Production mutation | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| 7 | `actor::prepare_pre_first_event_retry` — the retry gate drops the retryability check (`provider_error_allows_retry(&error) && …` → `true && …`), so a non-retryable 400 is re-issued instead of latching Errored. | `m4_retry_tests::m4_non_retryable_error_is_immediate_errored_without_retrying` | `assertion left == right failed: no re-issue` — `left: 2`, `right: 1`: the invalid-request error was retried (a second provider request went out). |
| 8 | `actor::wait_before_provider_retry` — the backoff ignores the server instruction (`error.retry_after_ms.unwrap_or_else(\|\| retry_backoff_ms(failed_attempt))` → `retry_backoff_ms(failed_attempt)`). | `m4_retry_tests::m4_retry_after_overrides_computed_backoff` | `assertion left == right failed: the server's Retry-After won` — `left: [1000]`, `right: [7000]`: the recorded wait was the computed 1s base, not the 429's 7s Retry-After. |
