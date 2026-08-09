# W-C — commands · notifications · export · retry (implementation notes)

Branch `wc-commands-notify-export` off v0.0.80. Four milestones, each its own
commit so an interruption preserves finished work. Companion:
`WC-commands-notify-export-mutation-notes.md` (8 executed kills).

## What shipped

- **M1 — custom slash commands** (`ab4d754`). Claude-Code-compatible
  `.haider/commands/*.md` (project walk-up) + `~/.haider/commands/*.md`
  (global). YAML frontmatter (`description`/`argument-hint`/`model`/
  `allowed-tools`, the last parsed-but-not-enforced), `$ARGUMENTS`/`$1..$N`/
  `$ARGUMENTS[N]` textual substitution, namespaced subdirs, project-wins
  precedence over global, malformed-file skip-with-warning, palette listing.
- **M2 — desktop notifications** (`e82d242`). OSC 9 on terminal + attention
  parks (never mid-stream), focus-gated (both branches — fires when focus is
  unreported), masked one-liner, tty-only (no bytes on a pipe), toggle,
  one-per-turn debounce.
- **M3 — session export** (`c2c8981`). `haider export` → markdown/json
  (native) + codex/claude-code/opencode (cross-harness), `--masked`. Codex
  rollout id == filename uuid; opencode SQLite INSERT behind `--confirm`,
  refused on a missing/locked db or a session collision.
- **M4 — API-error retry with a visible attempt counter** (`b37c5ad`, this
  wave). See below.

## M4 design (where the retry lives, and why)

The owner ask is Claude-Code-style automatic retry on API errors with a
visible `attempt K/10` counter during the backoff wait. The brief pointed at
`worker.rs`, but the daemon's MAIN-turn provider request is issued INSIDE the
core `HarnessActor`, which ALREADY owns the sole provider-retry site (R6):
`prepare_pre_first_event_retry` re-issues a request that failed BEFORE
emitting any stream event (the exact "no committed content" safety M4 wants),
honoring `retry_after_ms` and letting cancellation win the wait. A single
`fail-then-ok` fake-provider script is recovered THERE — a worker-level outer
retry would never observe it (core swallows the failure first) and would
double-count against R6. So M4 enhances that existing seam rather than adding
a second, fighting layer.

Concretely (all additive):

- **Protocol** — a new `RunState::Retrying { attempt, max, delay_ms, reason }`
  (`#[serde(tag="state")]`, non-terminal, parked). `Waiting {
  ProviderBackoff/RateLimit }` still exists and is untouched; only this
  variant carries the visible counter.
- **Core actor** — `wait_before_provider_retry` now commits `Retrying`
  (attempt = the NEXT try, so a first failure shows `attempt 2/10`) instead of
  a bare `Waiting`; `MAX_API_RETRIES = 10`; the backoff is the PURE
  `retry_backoff_ms(attempt) = min(30s, 1s·2^(attempt-1))` (jitter dropped so
  a law can assert the sequence), with `retry_after_ms` overriding it; the
  wait runs through an injected `RetrySleeper` (prod = real tokio sleep) so
  laws never wall-clock-wait. Non-retryable (400/401) and post-content
  failures latch `Errored` exactly as before; exhaustion latches `Errored`
  once.
- **TUI** — a warn/ember `retrying_line` (`✻ API error · Retrying in <N>s ·
  attempt <K>/<max>`) on the transcript-tail surface next to `thinking_line`;
  the plain/`--plain` badge renders the same string via the projection. The
  countdown reads the committed backoff (no new timer). `esc` during the wait
  cancels the turn (the actor's existing cancellation-wins-the-wait path).
- **M2 interplay** — `notify::attention_for(Retrying) == None`: a retry wait
  never fires a desktop notification; only the final `Errored` does.

Exhaustive `match RunState` sites the compiler surfaced were each given a
`Retrying` arm grouped with `Waiting` (active/mid-run): headless reducer
(no-op), delegation chip (Thinking), observe wire (Running), projection badge
+ tone (Restful), notify (None).

## M4 laws (`crates/haider-core/tests/m4_retry_tests.rs`)

- `m4_retryable_failure_retries_then_completes_with_visible_counter` —
  Overloaded → `Retrying{attempt:2,max:10,delay_ms:1000,reason:ProviderBackoff}`
  → Done; 2 requests; one recorded 1000ms wait.
- `m4_non_retryable_error_is_immediate_errored_without_retrying` — 400 →
  Errored, 1 request, no Retrying, no wait.
- `m4_retry_after_overrides_computed_backoff` — 429 Retry-After 7000 →
  Retrying delay 7000 (not the computed 1000), Done.
- `m4_exhausted_retries_latch_errored_once` — 10 Overloaded → Errored once,
  10 requests, 9 Retrying beats, recorded `[1000,2000,4000,8000,16000,30000,
  30000,30000,30000]`.
- `m4_backoff_schedule_is_a_pure_function_of_attempt` — asserts the exact
  sequence + purity + attempt-0 saturates to the base.
- `m4_failure_after_committed_content_is_not_retried` — EmitText then
  Overloaded → Errored, 1 request, no Retrying.
- `wc_notifications_tests::retry_wait_fires_no_notification` (+ the
  `attention_for` arm) — a retry wait stays silent; the terminal Errored fires.

## Discipline

- `cargo fmt --all -- --check` clean at every commit; CARGO_INCREMENTAL=0
  everywhere; named-path `git add` only.
- No real network in any test — the fake-provider seam (`Error` step with a
  kind + `retry_after_ms`) plus the recording `RetrySleeper` drive M4.
- Ledger: `2085 → 2128` (M1/M2/M3 tests accumulated without a baseline bump;
  updated once at the wave wrap-up).
- Probe ladder: **16/16 PASS** (14 demo + 2 live). `thinking_line`/`theme.rs`
  bytes were NOT changed (the retry line is a separate row), so the anim
  probe needed no update.
- Regression held green: G3 thinking-replay (core
  `provider_opaque_state_is_journaled_and_replayed_on_tool_follow_up`),
  responses-lite goldens, W-A task laws, W-B web laws, W-E shimmer laws.
