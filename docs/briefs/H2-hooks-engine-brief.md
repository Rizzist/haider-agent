# H2+H3 — hook engine + trust model (daemon-owned)

Second wave of the hooks block. Hooks react to JOURNAL FACTS — the
daemon's truth — never to display state. NO haider-tui (H4 adds the
screen).

## Scope

1. **Discovery**: `hooks.json` at workspace root via the B3 upward-walk
   discipline (canonical, symlink-refusing, bounded, nearest wins per
   name) + a profile-level `~/.haider/<profile>/hooks.json`. Schema:
   named hooks, each {matcher, kind, command, timeout_ms?,
   decision?}. Additive schema versioned `haider.hooks.v1`; malformed
   entries are skipped with an honest journaled notice, never fatal.
2. **Matchers on journal facts**: event-kind match plus optional
   filters (session, provider, run outcome, parked kind). Cover at
   minimum: session created, run started/parked(permission|input)/
   finished(outcome), subagent spawned/reported, compaction completed,
   update available, account expired. Additively, `user_message`
   matches the canonical committed `UserMessage` acceptance fact and
   accepts optional `mode` (`queue|steer`) and `has_attachments`
   filters. Matching happens on the
   daemon's committed envelope stream — after commit, never before
   (hooks OBSERVE truth; decision hooks are the one exception below).
3. **Exec hooks**: spawn with the event JSON on stdin, workspace cwd,
   clean env (NO secrets, NO tokens; explicit allowlist of vars),
   C1-era pre-exec fd sweep, bounded stdout/stderr (CAS overflow),
   hard timeout (default 30s, per-hook cap 300s), exit code + bounded
   output journaled as a hook-fired fact (additive kind).
   A `user_message` event is a sanitized `haider.hooks.v1` additive
   projection carrying session/run/branch, delivery mode, UTF-8 text
   bounded to 32 KiB plus `truncated`, and attachment metadata only:
   count and `{mime, bytes, artifact}` per attachment. `bytes` is the
   length and `artifact` is the BLAKE3 digest; resolved bytes are never
   serialized.
4. **Subscribe hooks**: one long-running process per hook, restarted
   with backoff on exit, receiving LF-framed envelopes matching its
   matcher; `user_message` uses the same sanitized bytes as exec;
   same hygiene; lifecycle journaled.
5. **Decision hooks (permission gate)**: for run-parked(permission)
   matchers with decision:true, the hook receives the pending effect
   description and may answer allow/deny on stdout within its
   deadline; timeout/absent/malformed → fall through to the normal
   Ask menu UNCHANGED. The decision rides the EXISTING EffectBroker
   answer path as a new journaled authority variant (additive) — the
   broker stays the single authority; a hook can never widen scope
   beyond what Ask could grant.
6. **Trust (H3)**: hooks are UNTRUSTED by default and never execute
   untrusted (journaled notice instead). Trust is digest-pinned
   (blake3 of the hooks.json bytes + per-hook command string); ANY
   digest change revokes. Grant/revoke: `haider hooks trust <digest>`
   / `revoke` / `haider hooks list --json` (observe.v1 style), a
   profile policy (trust_none | per_digest | trust_workspace), and
   `--trust-hooks` on headless runs (scopes to that run only). All
   trust changes are receipted commands (R2 pattern).

`UserMessage` parity is deliberately fact-based, not surface-based. TUI,
RPC, headless, and voice all converge on the same turn-acceptance transaction;
surface identity never enters the committed fact. Therefore one accepted
message yields exactly one hook event, and equivalent submissions have
identical JSON semantics across all four surfaces.

## Laws (minimum)

- untrusted_hook_never_executes_and_notices_honestly.
- digest_change_revokes_trust (edit between grant and fire).
- decision_timeout_falls_through_to_unchanged_ask (non-degenerate:
  hook answers allow in one fixture, times out in another, and the
  Ask menu bytes are identical to the no-hook world in the second).
- decision_hook_cannot_exceed_ask_scope.
- hook_spawn_inherits_no_descriptors (planted-fd probe, liveness-
  asserted per the spawn-hygiene pin precedent).
- hook_env_carries_no_secret_bytes (vault fixtures).
- exec_output_bounded_with_cas_overflow_and_journaled.
- subscribe_restart_backoff_bounded.
- matcher_fires_only_after_commit (crash between accept and commit →
  no fire on recovery replay unless the fact survived).
- hooks_facts_are_additive (goldens; unknown-kind tolerance).
- user_message_hook_fires_for_headless_and_rpc_submissions_identically
  (black-box production headless client + direct RPC, one durable fire per
  session, byte-identical event JSON apart from opaque ids).
- text_bounded_with_truncated_flag.
- attachment_metadata_never_carries_bytes.
- matcher_filters_respected.

Standing lane laws: tests never inline; mutation-notes with RUNTIME
failures (literals, non-degenerate fixtures, no self-referential
constants); CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings;
ledger; NO haider-tui; no Cargo.lock; no version bumps; leave
uncommitted; no git. Up to 3 research + 2 verify subagents.
