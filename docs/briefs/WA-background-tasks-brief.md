# W-A — long-lived background shell tasks + session completion messages

Owner contract, verbatim: "everything should be long lived, and when done,
like subagents, show session msg (like claude code). implement now."
Branch: `wa-background-tasks` (from main @ v0.0.77). This is daemon/tools/
TUI work; no provider-crate changes.

## Locked design decisions

1. CAPABILITY IS UNIVERSAL: the existing `process_exec` tool manifest
   gains optional `background: bool` (default false) and `name: string`
   (display label, defaulted from the command's first token). Foreground
   semantics are UNCHANGED (including the `!` composer escape — pin a
   regression law). A background call returns IMMEDIATELY with a typed
   result `{task_id, name, state: "running"}` so the model keeps working.
2. DAEMON TASK REGISTRY (per session): task_id (durable), pid/pgid,
   name, command summary, started_at, state {Running, Completed{exit},
   Failed{reason}, Killed}. Spawn through the EXISTING hardened spawn
   path (fd-hygiene close-sweep — the gate52 lesson lives there; child
   gets its own process group so kill = pgid kill). Registry state is
   JOURNALED as facts (task_started / task_completed with render.ui
   true, durable true, prompt bounded) — the journal is the truth, the
   in-memory registry is a projection, and daemon restart RE-ADOPTS or
   reaps orphans (pid liveness check + pgid kill on stale) — law-pinned.
3. OUTPUT: stdout+stderr tee into a bounded CAS artifact (the attachment
   store): keep a rolling tail preview (last 4 KiB) in the registry for
   cheap reads; on completion the full bounded output (cap 512 KiB,
   truncation marked honestly) becomes a CAS artifact referenced by the
   completion fact. No unbounded memory growth — law-pinned.
4. COMPLETION = SESSION MESSAGE (the owner's core ask): when a task
   finishes while the session is IDLE, the completion fact renders as a
   transcript row (subagent-report row pattern: name, exit status,
   elapsed, output tail preview) AND the next turn's prompt includes a
   bounded completion notice (prompt render like agent_messaged facts).
   When a RUN IS ACTIVE, additionally deliver a STEER-style injection via
   the existing nudge machinery (message_subagent precedent) so the model
   learns mid-turn — delivered_steer vs delivered_queued semantics copied
   from S1. Both paths law-pinned.
5. MODEL TOOLS (small, actor/broker-split like existing patterns):
   - `task_output { task_id, cursor? }` — bounded tail read with cursor,
     NOT an effect (no broker; request_input pattern).
   - `task_kill { task_id }` — IS an effect (broker class Kill or
     existing process ceiling), journaled intent/outcome.
   - Task LISTING: no separate tool — the completion/started facts plus
     `task_output` suffice; the registry snapshot rides the existing
     tool-inventory/observe surfaces if trivially exposible.
6. LIFECYCLE BOUNDS: tasks are SESSION-SCOPED. Session delete / daemon
   shutdown kills the pgid (fence law). A configurable hard cap (8
   concurrent background tasks per session) refuses honestly. Turn
   cancellation (esc) does NOT kill background tasks (they outlive turns
   BY DESIGN — that is the feature).
7. TUI: running tasks render as a live row/pill above the composer
   (subagent-row machinery: name, spinner, elapsed; completion row shows
   exit + tail). Plain mode prints equivalent lines. `/tasks`? NO new
   command this wave — the rows are ambient; revisit.
8. HEADLESS: `haider run` with a background task still exits when the
   TURN completes; the run summary notes still-running tasks and the
   daemon keeps ownership (they die with the session per 6 when the
   headless session closes — document this).

## Mandatory laws (runtime, non-vacuous)

- LT1 background call returns immediately; foreground unchanged; `!`
  escape regression green.
- LT2 completion fact journaled with bounded tail; full output artifact
  in CAS; truncation marker honest.
- LT3 idle completion → transcript row renders (projection test) + next
  turn prompt carries the notice (prompt compiler test).
- LT4 active-run completion → steer delivery observed (nudge count
  asserted — the W6 vacuous-pin lesson: assert the COUNT).
- LT5 task_kill through broker journals intent/outcome and the pgid
  dies; task_output cursor reads are bounded.
- LT6 orphan reaping: registry re-adoption on restart kills stale pgids
  (stage with a fake pid liveness seam, not real daemon restarts if the
  harness allows).
- LT7 concurrency cap refusal; session-close fence kills the pgid.
- Goldens: new fact kinds are additive (RawEnvelope — expect ZERO rpc
  frame changes; if the transcript grows anyway, regenerate honestly).

## Discipline

Standard lane rules: CARGO_INCREMENTAL=0; per-crate tests (haider-tools,
haider-daemon --test-threads=4, haider-core, haider-tui, haider-daemond);
fmt clean at every commit; named-path adds; ledger truthful before final
commit; notes + mutation-notes docs with ≥6 EXECUTED kills
(commit-before-mutation, "running 1 test" observed) covering: immediate
return, steer delivery, orphan reaping, output bounding, kill fence, cap
refusal. No version bumps/tags/MCP/renames; never touch the main
worktree; never delete ~/.codex/sessions.
