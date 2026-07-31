# W6c — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w6-c`, reviewed at 39d5f7f (frozen ref) plus
the reviewer's pin commits. Implementer: codex lane (gpt-5.6 xhigh) per
docs/briefs/W6c-supervision-brief.md.

## What shipped

Stall supervision (coordinator-owned, restart-safe: deadlines derive
from the delegations table + the child's last committed envelope): one
daemon-authored nudge after 120s of silence, cancel-with-stall-report
after a further silent deadline; the parent resumes with that report
like any other. Depth-capped recursion (limit 3) with a TYPED tool
error the model reads — never a run failure, never the sim's nested
dead-end; ancestry chains through the delegation rows. Parent cancel
sweeps the whole child subtree, stall and parent causes distinguished.

## The review's find: the vacuous recovery pin, three layers deep

The lane pinned pre-nudge progress but nothing pinned the POST-nudge
reset — and my first mutation there SURVIVED. Isolating it peeled three
distinct layers:

1. My first pin's timing put the nudge window BETWEEN the 25ms
   supervision polls — the nudge deterministically never fired, and the
   pin passed vacuously (nothing to cancel).
2. Strengthened with a nudge-must-fire assertion, the pin then exposed
   real mechanics: a nudge lands as a MID-RUN steer, so the child's run
   continues with one more provider round to answer it before
   terminalizing — by design, and invisible to every stalled-child test
   (those children never complete).
3. With the steer round served, the honest pin runs 3/3 green and KILLS
   the mutation: post-nudge progress genuinely averts the cancel — a
   nudge is a question, not a sentence.

## Mutations (reviewer-chosen, EXECUTED post-commit)

| # | Mutation | Result |
|---|---|---|
| M1 | recursion cap raised to 999 | KILLED (typed-error pin) |
| M2 | nudge skipped — first deadline cancels | KILLED (one-nudge pin) |
| M3 | post-nudge cancel ignores progress | SURVIVED twice against vacuous pins; KILLED by the honest recovery pin above |
| M4 | cancel sweep dropped | KILLED (orphan pin) |

Lane-authored pins also cover pre-nudge progress reset, restart re-arm
from durable state, and ancestry/depth chaining.

## Gate

Workspace clippy `-D warnings` clean; host suites store/tools/core/
daemon green (312 passed); full per-crate gate `gate29.out`; ledger
1171 → 1179.

## Honest residuals (non-blocking)

- Chip steer/close/answer RPCs remain the future UI-control patch.
- The nudge-answer text joins the child transcript; whether it should
  be excluded from the derived report summary is a taste call left for
  W6 polish (today the report is the last completed agent message of
  the run — the nudge answer can BE that message).

## Verdict

**SHIP** (merge to main, ships as v0.0.33 — W6 complete).
