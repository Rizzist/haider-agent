# Patch brief W2b/C2.1 — review round-1 fixes (8 P1s incl. the real turn loop)

Worktree /Users/rizzist/haider-run/haider-c2, branch w2b-c2. Findings in
docs/briefs/C2-review-1-NO_SHIP.md. Fix ALL EIGHT:

1. Unforgeable provenance: `PreAuthorized(UserTyped)` reachable ONLY via a crate-private
   provenance token minted by the shell-session entry point (sealed constructor —
   pub(crate) or a token type with no public constructor). Model-path calls can NEVER
   select it. Test: attempt from outside the sealed path must not compile OR must journal
   the normal permission flow (compile-fail test or API-shape test).
2. Cancellation law structural: once cancel is requested, EVERY supervisor error maps to
   Cancelled (the cancellation context wins over read/sink/CAS failures — carry the
   cancel flag into the finalizer decision). TERM/KILL failures escalate: KILL failure →
   the deadline is NOT cleared, outcome journals Cancelled-with-escalation-note and the
   registry entry is marked leaked (surfaced at close()); wait() must not hang close().
3. Streaming cap: enforce the inline cap PER-CHUNK as output arrives — once total exceeds
   the cap, stream chunks to a CAS staging writer incrementally (or spill file → CAS at
   end); memory holds at most cap + one chunk. Test with paused-clock output flood.
4. cwd TOCTOU: authorize and spawn against the SAME dirfd — open the canonicalized cwd
   O_DIRECTORY before digesting, verify identity (dev/ino) after authorization, spawn via
   that fd (fchdir in pre_exec or spawn helper). Same fd-anchor law as C4a fs tools.
5. Group containment: after the shell exits, sweep the process GROUP (killpg 0-probe →
   TERM → KILL residue) before journaling the outcome; document the setsid-escape
   residual honestly (descendants that new-session away are outside killpg — named
   residual, kernel-level containment is a later wave).
6. THE REAL TURN LOOP (architectural): a turn is a LOOP of provider requests.
   - TurnRequest/Message gains tool-result entries; the actor, on tool completion
     (incl. request_input answers), appends the result message and issues the NEXT
     stream_turn request; the loop ends on a finish with no pending tool calls.
   - FakeProvider: script becomes per-request — add `ExpectToolResult { call_id }` /
     next-request script segments so tests assert the answer REACHED request N+1.
   - Keep the item lifecycle laws; run-state transitions between requests stay inside
     the same run (Thinking between requests, no new run ids).
   - request_input's answer thus genuinely round-trips; assert the fake receives it.
7. Menu lifecycle on cancel: journal a durable MenuClosed/dismissal (protocol: reuse
   MenuAnswered with a timeout/dismissed via? NO — add nothing non-additive; emit
   MenuAnswered with via=Timeout is a lie. Correct: additive EventPayload::MenuClosed
   { menu, reason } with golden fixture — schema-additive, update projection to clear on
   it) + losing answer_menu() callers must not hang: reject stale answers immediately
   (the actor services the reject path while the turn runs, not only after drive_turn).
8. env-view redaction: classify names (KEY/TOKEN/SECRET/PASSWORD/CREDENTIAL/BEARER…)
   → value replaced by "•redacted" marker; non-secret values still shown; allowlist
   stays; test both classes.

Note: #7 adds an ADDITIVE protocol payload — regenerate golden fixtures; the schema-patch
rule applies at merge (C1 lane will rebase/union).

Gate: cargo test --workspace, clippy -D warnings (all targets), fmt --all --check,
xtask test-count --update, git diff --check. Leave changes uncommitted.
