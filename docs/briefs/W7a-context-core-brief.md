# W7a — context correctness: tree-compiled prompts, compaction, overflow, continuation

AUTHORITY: docs/research/w7-context-research.md (read WHOLE, first). Its
Q1 architecture binds; where this brief and the research disagree, the
research wins unless a law below says otherwise.

## Scope (W7a — correctness/recovery; thresholds/pre-announce are W7b)

1. **Activate tree compilation.** The provider prompt becomes the
   COMPILED PROJECTION of the durable tree (the frozen render-target
   law), replacing raw journal replay — byte-preserving for tool calls,
   tool results, and provider-opaque fragments (the research names the
   exact fragments that must survive verbatim; a diff-test proves the
   pre/post-activation prompts identical for an uncompacted session).
2. **Compaction as immutable ancestry substitution.** A daemon-authored
   internal summarize turn (the W6 internal-turn machinery) writes the
   FROZEN compaction-node shape; the compiler substitutes the summarized
   prefix; nothing is deleted. Durable compaction INTENT precedes the
   summarize (crash mid-compaction recovers or abandons cleanly — never
   a half-substituted prompt); the commit is CAS-aware against
   concurrent turns.
3. **Manual compact RPC.** `session.compact { command_id, session_id,
   worker_generation }` — receipt-backed like turn submit; emits the
   frozen compaction events the TUI's existing `⊟ COMPACTING` vocabulary
   renders; idempotent on replay.
4. **Overflow classification + forced compaction.** Classify provider
   context-exceeded distinctly (the research documents the OpenAI and
   Anthropic signatures, including Anthropic's conflation with
   MaxTokens — disambiguate as it prescribes). On overflow: forced
   compaction, then ONE retry of the same provider round in the same
   logical turn; a second overflow errors with a typed reason. Never a
   crash.
5. **MaxTokens continuation.** `FinishReason::MaxTokens` no longer ends
   the run: continue with another provider round (the research's
   continuation shape), bounded (const max continuations), events
   showing the seam.
6. **Windows reach the turn path.** The resolved provider carries the
   active model's catalog window + the reserved output budget
   (daemon-owned reserve; validate the TUI's requested cap against it).

OUT (W7b): threshold auto-compact, pre-announce, meter exact/estimated
truth, /tokens, /tree.

## Laws

As every lane: tests never inline; mutation docs with RUNTIME failures;
`CARGO_INCREMENTAL=0`; fmt + workspace clippy `-D warnings` clean; test
haider-protocol/store/core/daemon (sandbox socket failures expected —
host gate is authoritative); ledger update; protocol changes ADDITIVE
against the frozen shapes (they exist — USE them, do not fork them);
regenerate goldens if manifests change; no haider-tui; no Cargo.lock;
no versions; leave changes uncommitted; no git commands.

## Tests (minimum)

- Prompt-equivalence: journal-built vs tree-compiled prompts are
  IDENTICAL for a session with text, tool calls, tool results, and
  opaque fragments (mutation: compiler drops opaque fragments → fails).
- A compacted session's compiled prompt = summary node + post-compaction
  suffix, byte-stable across restart (mutation: substitution keeps the
  full prefix → fails).
- Crash between intent and summarize-commit: recovery abandons or
  completes, prompt never half-substituted (mutation: drop the intent →
  recovery test fails).
- Manual compact replay (same command_id) compacts once.
- Overflow → forced compact → retry succeeds; double overflow → typed
  error (mutation: retry unbounded → fails; mutation: overflow classed
  generic → fails).
- MaxTokens → continuation rounds, bounded (mutation: cap dropped →
  fails).

Use up to 3 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
