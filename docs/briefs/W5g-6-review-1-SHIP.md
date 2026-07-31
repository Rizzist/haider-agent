# W5g-6 — review of record #1 — SHIP

Reviewer: Fable 5 (implementer too). Branch `w5-g6`, reviewed at bc4f815
(frozen ref). Authority: the owner's live report ("why error out on
creating subagent? fix" + "i send another message it errors still") with
two screenshots, and the session's OWN event store.

## The root cause was never the subagent

The store told the whole story: turn 2 failed at THINKING — before any
tool call — with `provider_error — InvalidRequest: OpenAI HTTP 400`, and
so did every turn after. The only delta from the working turn 1: the
history now held an ASSISTANT text message. The Responses builder
replayed assistant history as `input_text`; the API accepts only
`output_text`/`refusal` there (its own words, confirmed live:
"Invalid value: 'input_text'. Supported values are: 'output_text' and
'refusal'"). **Every session died on its second turn.** No probe ever
ran two turns on one session — that is the gap, now closed by
`probe_two_turns.py` in the release ritual (sentinels DERIVED, never
contained in the prompt — the first draft of the probe was vacuous and
its mutation run exposed that before it could lie).

Also ruled out live before the store read: tool-call continuations
(both the id-less rebuilt `function_call` and the verbatim item are
ACCEPTED by the lite endpoint), so the tool loop needed no change.

## The three owner surfaces

1. **run_failed renders** — the reason was ALWAYS in the envelope; only
   the badge ever showed (three separate owner reports hit this).
   `TranscriptEntry::Error` now paints the wire code + message in err
   ink under the turn, in the TUI, `--plain`, and the demo store.
2. **Launcher caps at 4 recents** (owner ask): `LIVE_LAUNCHER_ROWS`
   9 → 4; `/sessions` lists the rest; digits 1-4 still reach every row.
3. **Resize clears stale hover**: a resize moves every target under a
   STATIONARY pointer, so the old hovered Hit repainted its highlight at
   the target's new row until the mouse happened to move.

## Mutations (reviewer-chosen, EXECUTED post-commit)

| # | Mutation | Result |
|---|---|---|
| M1 | assistant history back to `input_text` | KILLED twice: unit pin AND the live two-turn probe (turn 2 → ✗ ERRORED). First probe draft SURVIVED it — the sentinel rode the echoed prompt; probe fixed (derived sentinels), then both directions verified |
| M2 | `RunFailed` back to the swallowed arm | KILLED (transcript test) |
| M3 | launcher rows back to 9 | KILLED (cap test) |
| M4 | hover survives resize | KILLED (resize test) |

## Honest residuals (non-blocking)

- "spawn 1 subagent" still cannot spawn a subagent — subagent TOOLS are
  a locked-scope pillar not yet built; the model now gets to SAY that in
  text instead of dying.
- Imported-OAuth refresh (W5g-5 addendum) remains the next daemon lane.

## Gate

Workspace clippy `-D warnings` clean; TUI suite green; full per-crate
gate `gate20.out` (gate19's one red was the OLD 5-row reachability pin colliding with the new cap; the pin now encodes the policy); ledger 1146 → 1150.

## Verdict

**SHIP** (merge to main, ships as v0.0.28).
