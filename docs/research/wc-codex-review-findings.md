# W-C codex (gpt-5.6-sol xhigh) review findings — fix-first

Independent adversarial review of the W-C branch (M1 commands, M2 notify,
M3 export, M4 retry), 2026-08-09. Coordinator (Fable) verified the top
findings in-code before acceptance. Verdict: FIX-FIRST. codex explicitly
"checked OK": SQLite missing/locked handling + rollback, UUID collision/
path-traversal guards, codex filename/meta uuid + encrypted-reasoning
omission, claude slug + parent chain, partial-output/tool-side-effect
retry fencing, retry-delay cancellation + notification suppression,
substitution edges, RunState::Retrying projection precedence.

## HIGH (verify -> fix each; add a regression law)

H1. **opencode export omits message/part time columns** —
    export.rs:647,653: the `session` INSERT carries time_created/
    time_updated (they ARE computed, lines 532-533) but the `message`
    and `part` INSERTs OMIT them. The reduced test schema
    (wc_export_tests.rs:245) hid it (degenerate fixture). CONFIRMED: on a
    real opencode 1.17.20 db the first message INSERT fails, transaction
    rolls back, feature unusable. FIX: add time_created/time_updated to
    both INSERTs; make the TEST schema match the real one (NOT NULL time
    cols) so the law actually observes it.

H2. **401 credential-refresh infinite loop** — actor.rs:1981-1994: the
    `ProviderAttemptDecision::Retry` (credential refresh) arm returns
    Ok(()) WITHOUT setting `rotation_budget_consumed` and WITHOUT
    incrementing `provider_attempt`, so it bypasses the
    `provider_attempt < MAX_API_RETRIES` cap. CONFIRMED: a persistently-
    failing 401 whose resolver keeps deciding Retry loops forever. FIX:
    budget the refresh — a Retry decision must consume a bounded budget
    (e.g. one refresh, then fall through to the capped retry / Errored),
    so a non-recovering 401 terminates. Law: N consecutive 401s with a
    refresh-returning resolver end in Errored within a bound.

H3. **notification masking only masks @-tokens** — notify.rs:89: masks
    only whitespace tokens containing `@`; API keys, bearer tokens,
    paths pass unchanged into OSC 9 + OS notification history. CONFIRMED
    P1 regression. FIX: route the notification text through the real P1
    `mask_identity` secret masking (the same helper the surfaces use),
    not an ad-hoc @-only pass. Law: a title with `sk-...`/bearer is
    masked in the emitted OSC 9 bytes.

H4. **codex export source not listed by `codex resume`** — export.rs:317:
    writes `source:"export"` + copies Haider's provider verbatim; codex's
    rollout listing filters by an interactive-source allowlist + provider,
    so exported (esp. Anthropic-origin) sessions won't appear in
    `codex resume`. FIX: write an accepted interactive source value (and a
    provider codex will list, or document the resume path) so the export
    is actually resumable. Law/assert: the emitted session_meta source is
    in codex's accepted set. (If codex's allowlist can't be satisfied
    honestly, DOCUMENT the limitation instead of faking it.)

## MEDIUM (fix the real ones; law where it makes sense)

M1. export.rs:376 — codex `function_call` items with possibly-non-JSON
    `arguments` and NO matching `function_call_output` -> orphaned/
    malformed call breaks a resumed turn. FIX: emit valid paired
    call+output, or omit tool items from the export (a clean transcript
    beats a broken-resume one) — decide and note.
M2. export.rs:834 — collision refusal is check-then-write TOCTOU; use
    `OpenOptions::create_new` (atomic, symlink-safe), not exists()+write.
M3. export.rs:1116 — codex rollout + history.jsonl are two ops w/o
    rollback; a failed history write leaves the rollout and blocks retry.
    FIX: write history first or make the pair recoverable/idempotent.
M4. export.rs:1026 — export collects the whole replay through an
    unbounded channel -> multiple full-session allocations; a huge/hostile
    session OOMs. FIX: bound the buffer / stream the projection.
M5. custom_commands.rs:160 — "YAML frontmatter" parsed with
    split_once(':'); malformed YAML is SILENTLY ACCEPTED instead of
    skip-with-warning (the brief REQUIRED skip-with-warning). FIX:
    validate; a malformed frontmatter file is skipped with a surfaced
    warning. Law.
M6. custom_commands.rs:149 — body offset counts each line ending as 1
    byte but str::lines() strips 2-byte CRLF; a CRLF command bleeds part
    of its closing `---` fence into the prompt. FIX: CRLF-correct offset.
M7. custom_commands.rs:316 — project discovery follows a
    `.haider/commands` root symlink without canonical-path containment;
    an untrusted checkout can symlink it outside the repo to load
    external/huge files. FIX: canonicalize + containment check.
M8. custom_commands.rs:373 — file limit applied AFTER collecting+sorting
    all entries, and files read without a size cap; millions of files or
    one giant .md stalls/OOMs TUI startup. FIX: bound the walk + per-file
    size cap.
M9. app.rs:8719 — a launcher custom command with `model:` doesn't queue
    the model change before CreateSession (8775); first turn uses the old
    pair. FIX: apply the override before the first turn (or before create).
M10. app.rs:8899/9375 — terminal-notification eval only in the ACTIVE
     session reducer; background/parked-branch turns bypass it. FIX:
     evaluate on any session's terminal transition (respecting focus).

## LOW

L1. actor.rs:2227 — retry wait observes the turn cancel token but not the
    actor stop channel; stopping during a 60s Retry-After blocks shutdown
    for the full delay. FIX: also select on the stop signal.

## Handling

Fix all HIGH + M1-M8 (correctness/security/robustness). M9/M10 are TUI
behavior — fix (logic, not layout). L1 fix if cheap. Each fix: verify the
finding in-code first, fix minimally, add/update a regression law, run it
green; then an executed mutation kill for the HIGH ones (H1-H4) at least.
