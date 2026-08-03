# H1 — the observe surface: scriptable daemon truth (haider.observe.v1)

First wave of the hooks block. The daemon journals everything; this
wave EXPOSES it as a typed, versioned, automation-consumable CLI
surface. NO haider-tui.

## Research-first (in-lane)

Map what the wire already serves before adding anything: session.list/
session.read, provider.list, tools.inventory, Welcome features, the
W9 update machinery (haider update's staged check), account state.
Add AT MOST two additive RPCs where digests don't exist today
(a per-session state digest; update.status). Additive protocol only,
goldens updated, older-client tolerance re-proved.

## Scope (haider-cli + haider-client + minimal daemon/rpc)

1. `haider status [--json]`: daemon version + generation, update
   availability (staged W9 check — never mutates), advertised
   features, active account (provider + alias, NO secrets), session
   count, profile path. Exit 0; exit 69 if no daemon and --no-spawn.
2. `haider sessions [--json]`: per session — id, title, run state
   (idle/running/parked_permission/parked_input/errored/cancelled),
   active branch (client cannot know TUI display state — report the
   daemon-known branches + main), provider/model, footprint
   (exact/estimated + tokens), subagent count, updated_at.
3. `haider session <id> [--json]`: depth view — everything above plus
   pending menus (kind + title, NO option secrets), parked permission
   descriptions, subagent chips (callsign policy: daemon names only —
   roster is TUI display), branch list with heads, last N event kinds.
4. `haider session <id> --watch` and `haider events [--follow]`:
   LF-framed raw envelope JSONL (haider.run.v1 framing precedent);
   --follow tails ALL sessions (attach per session, session-global
   cursors); forward-compat law: consumers tolerate additive kinds —
   document it in --help text.
5. Every human format has a --json twin with a STABLE schema tagged
   `haider.observe.v1` + object kind field; JSON is the contract,
   human text is free to change. Exit codes reuse the haider.run
   table.

## Laws (minimum)

- observe_json_schemas_are_goldened_and_additive.
- status_reports_update_availability_without_mutating (staged check
  leaves no marker/lock).
- sessions_reports_parked_states_distinctly (permission vs input —
  non-degenerate fixture: both parked kinds present at once).
- session_depth_never_leaks_secret_material (vault/oauth fixtures).
- watch_streams_are_lf_framed_raw_envelopes_and_tolerate_additive_kinds.
- exit_codes_match_the_headless_table.
- no_daemon_paths_are_typed (69) not panics.

Standing lane laws: tests never inline; mutation-notes with RUNTIME
failures (literals, non-degenerate fixtures); CARGO_INCREMENTAL=0;
fmt + workspace clippy -D warnings; ledger; NO haider-tui; no
Cargo.lock; no version bumps; leave uncommitted; no git. Up to 3
research + 2 verify subagents. Finish with files/tests summary.
