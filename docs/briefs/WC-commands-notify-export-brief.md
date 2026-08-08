# W-C — custom slash commands + desktop notifications + session export (incl. cross-harness)

Owner contract: "Custom Slash commands also important. do this as well.
Session export should be exportable, and we should export our core to any
harness (codex, claude code, open code) so we need export support and
export specific support." + (from the wave list) desktop notifications.
Authority: `docs/research/wc-harness-export-research.md` (the three
foreign session formats, mapped from the REAL local stores). Branch:
`wc-commands-notify-export` off v0.0.80.

THREE ORDERED MILESTONES — commit after EACH so an interruption preserves
completed work (this run has seen many lane deaths). Do them in order.

────────────────────────────────────────────────────────────────────────
## M1 — custom slash commands
────────────────────────────────────────────────────────────────────────

1. SOURCES: project `.haider/commands/*.md` (workspace root, walked up
   like project instructions) + global `~/.haider/commands/*.md`. Project
   wins on name collision; both merge OVER (never replace) the built-in
   command registry. A subdirectory is a namespace
   (`.haider/commands/git/commit.md` → `/git:commit`) — CC convention.
2. FORMAT = Claude-Code-COMPATIBLE (decision: users drop their existing
   `.claude/commands/*.md` in unchanged). YAML frontmatter keys honored:
   `description` (shown in the palette), `argument-hint` (completion
   hint), `model` (optional per-command pair override — validated against
   the catalog like /model; ignored with a note if unknown), `allowed-tools`
   (PARSED and stored but NOT enforced this wave — note it; enforcement is
   a follow-up). Body = the prompt template.
3. ARGUMENT SUBSTITUTION: `$ARGUMENTS` (all args, space-joined), `$1`..`$N`
   (positional), and `$ARGUMENTS[N]` tolerated as an alias for `$N`.
   Unfilled positionals expand to empty. A `$` not followed by a token is
   literal. Substitution is textual and happens at expansion time.
4. NO INLINE EXECUTION: Claude Code's `` !`cmd` `` inline-shell syntax is
   OUT this wave — a custom command expands to a PROMPT ONLY (a user turn
   with the substituted body). Document this. (If a command names a
   `model`, the expansion carries a per-turn pair selection — reuse the
   G3/model selection path; do NOT invent a new one.)
5. DISCOVERY: the command registry (commands.rs COMMANDS + the dynamic
   layer W-A/G1 already grew) gains a loaded-file layer. Loading is
   daemon-side or client-side per where the registry authority lives —
   FOLLOW the existing pattern; a malformed file is skipped with a
   surfaced warning (never a crash). `/help` and the palette list custom
   commands with their descriptions, visually distinct from built-ins.
6. LAWS: parse (frontmatter + body, CC file drops in), namespacing,
   project-over-global precedence, all three substitution forms +
   empty-positional, malformed-file skip-with-warning, palette lists them,
   `model` override reaches the turn (or is ignored-with-note if unknown).

────────────────────────────────────────────────────────────────────────
## M2 — desktop notifications
────────────────────────────────────────────────────────────────────────

7. TRIGGER STATES (from the run-state machine — the daemon already knows
   these): a turn reaching a TERMINAL state (Done / Errored) AND a
   permission/attention park (PermissionRequired / InputRequired /
   WaitingDevice). NOT on every stream chunk. Debounce so one turn yields
   at most one "done" notification.
8. FOCUS GATE: fire ONLY when the terminal is UNFOCUSED. Enable crossterm
   focus reporting (FocusGained/FocusLost events) and track focus in the
   TUI model; if the terminal does not report focus, fall back to firing
   regardless (better a redundant ping than a missed one) — pin both
   branches.
9. MECHANISM: OSC 9 desktop notification (`ESC ] 9 ; <text> BEL`) — zero
   dependency, supported by iTerm2/kitty/WezTerm/etc. Emit through the
   same terminal writer the TUI already owns; the text is a bounded,
   MASKED (P1) one-liner ("haider: turn done in <session-title>" /
   "haider: needs your approval"). Never emit secrets. A settings toggle
   (`notifications: on|off`, default on) gates it; honor any existing
   reduced-noise setting.
10. HEADLESS: `haider run` gets the same attention signal via a BUILT-IN
    hook on the terminal/park states (the hooks engine already exists —
    register a default notify hook, OSC 9 to stderr's tty when
    interactive). Document that non-interactive/piped runs emit nothing.
11. PLAIN/probe: no OSC bytes in piped (non-TTY) output — pin it (a probe
    that greps for a stray `ESC]9` in captured output must find none when
    not a tty).
12. LAWS: fires on terminal + park states only (not mid-stream), focus
    gate both branches, masked text (no secret leak), toggle off →
    silent, non-tty → no OSC bytes, debounce (one per turn).

────────────────────────────────────────────────────────────────────────
## M3 — session export (+ cross-harness)
────────────────────────────────────────────────────────────────────────

13. COMMAND: `haider export <session-id> [--format FMT] [--out PATH]
    [--masked]`. FMT ∈ {markdown (default), json, codex, claude-code,
    opencode}. The journal IS the transcript — export is a pure rendering
    pass over the durable facts we already store (no new capture). `--out`
    default: stdout for markdown/json, a sensible file path for the
    harness formats. `--masked` applies the P1 masking pass (our
    streamer-safe differentiator — neither competitor exports masked).
14. NATIVE formats:
    - markdown: readable transcript (user/assistant turns, tool calls
      collapsed, timestamps). This is the shareable artifact.
    - json: structured — the fact list projected to a stable public
      schema (NOT the raw envelope; a documented export schema).
15. CROSS-HARNESS writers — from `wc-harness-export-research.md` MINIMUM
    record sets (verified against real local stores):
    - **codex**: `sessions/<Y>/<M>/<D>/rollout-<ts>-<uuidv7>.jsonl`;
      line 1 `session_meta` (id==filename uuid, cwd, cli_version,
      originator), then `response_item` message records (user/assistant).
      OMIT reasoning `encrypted_content` — never fabricate. Also append a
      `history.jsonl` line. The uuid-v7 + timestamp are INPUTS (scripts
      can't call Date.now) — accept them as args or derive from the
      session's own created_at fact; do NOT invent wall-clock in code
      paths that must be deterministic for tests.
    - **claude-code**: `projects/<cwd-slug>/<sessionId>.jsonl`, slug =
      cwd with `/` and `.` → `-`; uuid/parentUuid-chained user+assistant
      records (assistant `message` is the NATIVE Anthropic message shape
      — HIGHEST fidelity since we already speak it), `isSidechain:false`,
      cwd/version/timestamp; add `ai-title` + `last-prompt` for a good
      picker row.
    - **opencode**: guarded SQLite INSERT into
      `~/.local/share/opencode/opencode.db` (WAL) — session + message +
      part(text) rows, ids in COLUMNS. BEHIND AN EXPLICIT FLAG/CONFIRM;
      REFUSE cleanly if the db is missing or locked (never corrupt a
      live db). This is the one writer that mutates a foreign app's
      store — treat it as the riskiest and gate it hardest.
16. TARGET DIRECTORY SAFETY: writing into `~/.codex` / `~/.claude` /
    opencode's dir means the export lands where that harness lists it.
    Default to writing under the target harness's real dir so
    `codex resume` / `claude --resume` find it, but NEVER overwrite an
    existing session file (unique id; refuse on collision). Document the
    exact paths written.
17. LAWS: markdown + json render from a fixture journal; --masked hides
    identity (P1 helper); codex rollout record set is valid + resumable
    shape (assert the session_meta line + a message record, id==filename);
    claude-code cwd-slug + uuid-chain shape; opencode INSERT into a temp
    db then read back the rows (rusqlite/sqlite in a tempdir — NEVER the
    real db in tests); collision refusal; missing/locked opencode db
    refused cleanly.

────────────────────────────────────────────────────────────────────────
## Discipline (all three milestones)
────────────────────────────────────────────────────────────────────────
- CARGO_INCREMENTAL=0 everywhere; per-crate tests as touched (likely
  haider-cli, haider-tui, haider-daemon `-- --test-threads=4`,
  haider-core, haider-store; a new small crate ONLY if the export writers
  genuinely need isolation — prefer a module in an existing crate).
- NO real filesystem writes to the user's real ~/.codex, ~/.claude, or
  opencode db in tests — tempdirs/temp sqlite only. NO real network.
- `cargo fmt --all -- --check` clean at every commit; named-path adds
  only; commit after EACH milestone (M1, M2, M3) plus the notes commit.
- Ledger `cargo run -p xtask -- test-count --update` before the FINAL
  commit (baseline 2085); truthful old→new.
- `docs/briefs/WC-commands-notify-export-notes.md` +
  `docs/briefs/WC-commands-notify-export-mutation-notes.md` with ≥6
  EXECUTED kills (commit-before-mutation, single `python3` anchor with
  `count==1`, one named test with "running 1 test", observed failure,
  revert, green) covering AT LEAST: argument substitution, project-over-
  global precedence, the focus gate, the non-tty OSC suppression, the
  codex id==filename invariant, and the opencode collision/locked refusal.
- If a TUI-visual element changed (palette listing of custom commands),
  run `scripts/tui-probes/ladder.sh` before done.
- Do NOT: bump versions, tag, add MCP, rename existing types, delete
  anything under ~/.codex/sessions or the user's real ~/.claude. Disk is
  tight (~6G) — STOP and report on any "No space left on device" rather
  than retrying.
