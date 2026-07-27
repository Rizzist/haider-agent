# TUI3 review round 2 — NO_SHIP

- Reviewer: gpt-5.6, 2026-07-27. Frozen SHA a276d8b (scope 5f1cdd6..a276d8b).
- ALL 14 round-1 findings CLOSED except P2-12 PARTIAL. Owner complaints 2 and 6 now FULLY CLOSED.
- ADJUDICATION ACCEPTED: the reviewer read tui.js and confirmed the implementer's pushback —
  the sim's `interrupt` touches only the parent run token/queue/note (tui.js:921/1277/1551), so
  children DO outlive a cancelled parent turn; cancelling chip arms would itself be a divergence.
  Also confirmed: "ascii" does NOT match /ci\b/; "pci" does.
- Full log: ~/haider-run/tui3-review-r2.log

## New findings (round 3)

P1
1. Never-cancelled CONTROL_ARM envelopes mutate a REPLACEMENT session (runtime.rs:291/536/874,
   app.rs:1875/1805): menu answers are emitted under the never-cancelled control tag and consumed
   regardless of which session rendered the card; `fresh_session` neither cancels nor identifies
   buffered control answers, and fixed `voice-card`/`tools-card` IDs apply consequences
   unconditionally. Repro: answer an old /voice card → Back/attach a sample before the answer is
   consumed → the stale answer changes the REPLACEMENT session. Sim promises capture the original
   session/block ids (tui.js:850/1824).
2. Session MenuOption stays active after leaving the session surface (app.rs:1958, hover :2132):
   the non-subagent branch validates `projection.open_menu` but NOT `screen == Session`, and Back
   leaves the projection/card intact → a queued click on the old option rect answers a
   now-invisible card and starts its parked continuation.

P2
3. Launcher Talk fabricates a session (app.rs:1343/1374/1987): sim renders the launcher mic but
   `speak` RETURNS when there is no active session (tui.js:2044/3036).
4. TalkChip and HelpHint still lack owning-surface guards (app.rs:1987) — a stale mic hit after
   entering Aura creates invisible listening state; a stale launcher help hit opens Help on Session.
5. Aura is now OVER-cancelled vs the sim (runtime.rs:662/674): sim `/clear` and main-session
   interrupt do NOT advance auraRunRef (tui.js:1913/1950/2060); only reset/replacement does. Our
   StopScripts cancels every owner, so a background orchestration vanishes where the sim finishes it.
6. Auto-title remains interrupt-sensitive (open half of P2-12): sim's 1.5s timeout still lands
   after an interrupt; ours cancels it.
7. **Workspace gate is FLAKY** (pre-existing, outside the TUI delta): process-cancellation outcome
   assertions at haider-tools process_tools_tests.rs:658 and :733 failed on the first workspace run,
   one failed again in isolation, then passed. The required release gate is nondeterministic.

P3
8. Stale comments contradict correct behavior: script.rs:527 + tui31_lifecycle_tests.rs:657 claim
   "ascii" matches /ci\b/; render.rs:401 documents obsolete launcher counts; script.rs:1908 still
   describes the reverted nested "intended flow".
9. Counts overstated: the suite grew 368→385 = 17 new tests, not 18 (no deletion; the P2-14 rename
   is legitimate and `aura_orchestrates_spawn_and_status_with_talk_and_toggles` is intact and
   equivalent to 5f1cdd6). Two cases call `respond_beats` directly instead of production dispatch
   (tui31_lifecycle_tests.rs:637/661).

VERDICT: NO_SHIP — two P1 stale-owner/stale-surface paths can still land hidden menu actions in a
replacement or no-longer-visible session.
