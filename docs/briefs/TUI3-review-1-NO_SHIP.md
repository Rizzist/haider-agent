# TUI3 review round 1 — NO_SHIP

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-27
- Frozen SHA: 5f1cdd6 (scope 8dcfd0c..5f1cdd6: TUI3a 29163b5 + TUI3b 1/2 268b926 + TUI3b 2/2 5f1cdd6)
- Full log: ~/haider-run/tui3-review-r1.log

## Owner complaints
CLOSED: (1) composer band — tokens preblend exact per theme, status ground now page ground;
(3) SessHead anatomy; (4) boot checks verbatim; (5) mark/shahada inks + honest size deferral.
PARTIAL: (2) launcher — anatomy fixed, but liveness fabricated wrong + metas abbreviated (P2-8);
(6) hover — works everywhere EXCEPT chip-question rows (P2-7).

## Findings

P1
1. Chip scripts survive interrupt: chip_gen deliberately kept alive while pending_arms is
   cleared → later transcript/state/parent-note mutations; answering a chip question after
   interrupt closes the menu but cannot run ChipResolve → chip PERMANENTLY BLOCKED; closing an
   active chip only marks it closed/removing while its script continues; ChipScript can adopt
   the current generation in a teardown race. (runtime.rs:372/580/1102, app.rs:1396)
2. Aura survives Esc//clear/fresh-session (aura_gen bumped ONLY by /reset; StopScripts does not
   bump it) → hidden Aura keeps mutating roster/log/transcript; aura_submit leaves state Idle
   until an async beat so two rapid submits overlap under one generation. (runtime.rs:670,
   app.rs:1235/1362)
3. Talk timer fires after Esc: click Talk sets listening (not turn_active), idle Esc navigates
   without interrupting the 1.3s timer → TalkFire calls fresh_session from the Launcher and
   yanks the user into a new canned session. (app.rs:1169/1328, runtime.rs:603)
4. Chip item-ID reuse: chip_stream/chip_tool derive IDs from agent + fixed g1/g2/n1/n2/n3;
   projection permanently rejects closed IDs → a SECOND message to a completed chip shows the
   user row but silently drops all assistant/tool output. Sim uses fresh nid(). (script.rs:1413/
   1785, projection.rs:241, tui.js:1097)
5. New hit actions lack owning-surface guards (chip rows, subtree toggle, chip close/crumb, all
   Aura actions): a stale rect can close a hidden chip or start hidden Aura work before redraw —
   violates the stale-frame law the handler itself documents. (app.rs:1947 vs :1885)
6. Aura height ledger: status yields only for Session/Subagent; Aura hard-allocates bar/rules/
   orb/columns/composer → at 90×1 Aura input is INVISIBLE BUT ACTIVE; 90×5 entered Aura with no
   bar painted. (render.rs:35/1365)

P2
7. Chip-question click AND hover dead — rows emit MenuOption hits but handlers validate only
   projection.open_menu(), never viewed_chip().question_menu(). (render.rs:1336, app.rs:1920/2063)
8. Launcher liveness fabricated wrong (Rust marks cellular-pool-fix running; the sim's L1 seed
   owns the running web-index chip and shows "1 live subagent") + Aura/Accounts/Peers metas
   abbreviated vs sim. NOT genuinely blocked by /sessions — SampleSession already carries
   liveness. (mock.rs:276, render.rs:390, tui.js:556/3278)
9. Launcher hits still carry mutable ordinals (AttachSample(usize), ExtraRow(u8)) resolved
   against current state — contradicts the value-carrying-hit law. (app.rs:621)
10. voice_live never cleared when a branch parks on AwaitMenu (Voice(false) sits after the menu
    return) → later ordinary rows render as ♪ speaking. (script.rs:700, runtime.rs:1070)
11. Token/routing law divergences: Rust counts Unicode scalars, sim uses UTF-16 .length (emoji
    9 vs 18); leading-boundary ci/subagents checks differ from /ci\b/ and /subagents\b/ ("ascii"
    routes generic in Rust, test in sim); generic/roster counters advance while BUILDING beats
    before the 750ms alive boundary → an interrupted thinking phase skips the next intro/callsign.
12. Auto-title assigned at submit instead of inside the 1.5s callback (sim sets title AND note
    in the callback). Helper itself matches. (app.rs:1516, tui.js:1219)
13. Compaction: `compact-{before}` ID reused when /compact runs twice without token growth →
    second row dropped; auto-compaction omits the sim's IDLE → 30ms → COMPACTING transition.
    Threshold + before/6%-after numbers correct. (script.rs:1361, tui.js:1507)
14. Nested chip delegation ports the "intended" flow; the sim's cTool returns undefined so it
    dead-ends with parent streaming + child running. Instruction says tui.js wins.

## Fidelity spot-check
MATCH: subagent(s) primary beats, crash/recovery, auth/delegation, custom-tool 3 modes, plan-todo,
submit preprocessing order, generic/test rotation law for uninterrupted turns.
DIVERGES: prod/permission (voice cleanup only), test/flake (ci\b routing), rate-limit (selected
account not patched), generic (non-BMP token counts).

VERDICT: NO_SHIP — stale chip/Aura/Talk work, repeated chip item IDs, stale hidden hit actions,
and an Aura ledger permitting invisible-but-active controls.
