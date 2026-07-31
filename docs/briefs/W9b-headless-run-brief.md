# W9b — daemon-backed `haider run`: headless transaction, permissions, output laws

AUTHORITY: docs/research/w9-updates-headless-research.md (read WHOLE,
first) — §Q1/§Q2/§Q4 W9b1+W9b2 bind. Central law: the existing
in-process SQLite/`HarnessActor` `run` is a SECOND AUTHORITY and must be
MIGRATED to daemon RPC, preserving its pinned JSONL and exit-code laws
(the old tests are migration oracles, not casualties).

## Scope (haider-client/haider-cli/protocol/rpc/daemon — NO haider-tui)

1. **Reusable headless transaction** in haider-client: ensure_daemon
   (ClientKind::Headless, View+Control, live features) → SessionCreate →
   Control attach (submit NEVER precedes attach) → TurnSubmit → cursor
   stream reduction (at-least-once dedup, gap → reattach from last
   applied, lost_events() → cursor reattach) → terminal reduction
   (Done | Errored+RunFailed | Cancelled ONLY — parked states never end
   the runner). Buffer events by run_id until the submit response
   correlates (wire disclaims socket-order causality).
2. **Timeout/cancel**: --timeout wall clock → durable turn.cancel once →
   bounded terminal grace → timeout outcome (exit 124) even when the
   terminal is Cancelled. Disconnect is NOT cancellation.
3. **Permission policy**: on MenuOpened(Permission), select the
   server-enumerated option with typed decision RejectOnce (never by
   index/label), emit the denial notice, continue to terminal (denied
   tool → Done is exit 0 with the denial exposed in machine output).
   Non-permission InputRequired + EffectOutcomeUnknown: never guessed —
   cancel, exit 77 typed. NEW additive seam
   `SessionPermissionOverridesV1` on SessionCreate + SessionMetadataV1
   (typed booleans: allow_writes, allow_exec → FsWrite/ProcessExec
   Allow), in the create digest, persisted, applied AFTER registry
   defaults; journals ordinary policy Allow (NEVER forged
   PreAuthorized(UserTyped)); Welcome advertises
   `session_permission_overrides_v1`; the flags REFUSE (exit 76) when
   the daemon lacks the feature.
4. **CLI surface** (extend the manual parser, no clap): `haider run
   <prompt> [--output print|json|jsonl] [--timeout <dur>]
   [--allow-writes] [--allow-exec]`, default print. Route EVERY mode
   through the daemon-backed runner; DELETE the in-process actor path.
   - print: final assistant text + one LF on stdout; progress/denials/
     errors on stderr; no ANSI, no TTY.
   - json: one LF-terminated `haider.run.v1` object (schema, session_id,
     run_id, outcome done|errored|cancelled|timeout|input_required,
     response, usage, permission_denials[], error) — additive-only in v1.
   - jsonl: the frozen RawEnvelope-per-LF contract, monotonic reduced
     seqs, correlated terminal line (existing fixtures are laws).
5. **Exit codes** (research table): 0 done · 2 usage · 65 provider ·
   69 daemon-unavailable · 70 internal · 74 output I/O (BrokenPipe
   deliberate) · 76 protocol/feature/version · 77 blocked
   input-required · 124 timeout · 130 user cancel. Table-driven test.

## Laws

Standing lane laws (tests never inline; mutation docs with RUNTIME
failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings;
protocol ADDITIVE; goldens regenerated if manifests change; ledger
update; no haider-tui; no Cargo.lock; no versions; leave uncommitted; no
git). The research's W9b1+W9b2 "Minimum laws" bind verbatim. Sandbox
socket failures expected — host gate authoritative.

Use up to 3 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
