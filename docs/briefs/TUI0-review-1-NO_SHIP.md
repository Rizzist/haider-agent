# TUI0 review round 1 — NO_SHIP (gpt-5.6, frozen d831d3e)

codex
NO_SHIP. Two P1 defects affect the acceptance path and terminal restoration.

## Findings

- **P1 — Demo completion causes a permanent hot loop.** `runtime.rs:115` keeps polling the closed envelope receiver; `None` repeatedly sends `StreamEnded`, and `app.rs:92` marks every no-op dirty. The loop spins while redrawing at 30 fps. A PTY run consumed 12.68s user CPU in 20.04s. Fuse/disable the receiver branch after the first `None`.

- **P1 — Terminal setup is not transactional.** `runtime.rs:31` enables raw mode before `runtime.rs:32` can fail entering alternate screen or enabling paste. That error returns before a guard exists, leaving raw mode—or a partially entered alternate screen—active. Establish rollback immediately after enabling raw mode.

- **P2 — Raw projection ignores `RenderTargets.ui`.** `projection.rs:137` applies every payload regardless of `envelope.render.ui`. Events explicitly marked non-UI can mutate badges, transcript, menus, and usage. Sequence accounting should advance, but display mutation must be skipped when `ui == false`.

- **P2 — Completed item IDs are not idempotent.** `projection.rs:233` replaces only a still-streaming block; a later `Completed` for the same ID appends a duplicate at line 241. Plans similarly append every all-done completion at `projection.rs:225`, while a later `Started` can repin a closed ID. Exact duplicate sequence numbers are skipped, but lifecycle duplication under a new sequence violates replace semantics.

- **P2 — Blocking menus are invisible and unanswerable in the live TUI.** `render.rs:160` always renders the composer and never consults `open_menu()`. `app.rs:120` also treats Enter as composer input and exposes no answer side effect. A `MenuOpened` run can therefore block with no usable interaction, unlike the sim’s composer-replacement menu.

- **P2 — Command-output honesty is not surfaced.** `render.rs:277` ignores both `output_truncated` and `output_decode_error`. The plain renderer reports truncation but still ignores decode failure at `plain.rs:117`. Invalid chunks are silently omitted, allowing incomplete output to appear authoritative.

- **P2 — Plain-output write failures panic instead of returning a defined exit code.** `main.rs:117` uses `print!`, which panics on BrokenPipe. Reproduced with a closed pipe. Use fallible stdout writing and map the result deliberately—normally BrokenPipe success or `EX_IOERR`.

- **P3 — The terminal guard is not nestable and permanently stacks panic hooks.** `runtime.rs:33` installs a hook that Drop never restores. Re-entry nests hooks, and dropping an inner guard leaves an outer guard believing the alternate screen is still active. This is outside the one-shot CLI path but conflicts with the public guard API.

- **P3 — Follow-bottom stops beyond 65,535 wrapped rows.** `render.rs:134` saturates total height to `u16::MAX`, so later content becomes unreachable. This is accurately recorded as deferred in [OPTIMIZATIONS.md:17](/Users/rizzist/Documents/CODING/haider-agent-tui0/docs/OPTIMIZATIONS.md:17), but remains a correctness limitation.

- **P3 — `fmt_tok` is not exactly JS-compatible at decimal half boundaries.** `format.rs:16` implements mathematical half-up rounding, while the sim uses JavaScript `toFixed`. For example, Rust returns `1.2k`/`1.2M` for 1,150/1,150,000; the sim returns `1.1k`/`1.1M`. The parity tests omit these boundaries.

- **P3 — Narrow status bars sacrifice the badge to the meter.** `render.rs:198` reserves the full meter width with no narrow-mode policy. When it exceeds the terminal width, the badge/model side can collapse to one cell. Hide or compact the meter before clipping primary state.

- **P3 — Badge styling does not match the sim’s state vocabulary.** `style.rs:59` and `render.rs:203` render every state as bold gold-filled. The sim uses outlines for idle/starting/waiting and maroon/warn/error fills for tool, compacting, and errored states.

- **P3 — “Snapshots” do not pin layouts.** `app_render_tests.rs:57` and related tests use substring assertions rather than exact buffers, coordinates, or style goldens. They do not cover menus, command honesty, exact-fit scrolling, narrow status bars, large todo panels, or every screen in every theme. No test was deleted from baseline 128; the narrow sanctum assertion was modestly weakened to ignore short words, though the direct `fit` test partly compensates.

Confirmed sound: interleaved non-plan item deltas route by ID; delta-after-completion is counted; sequence duplicates/gaps and unknown payloads are tolerated; command-tail length/cap math is bounded; normal `Terminal::new`/draw/quit paths restore through Drop; bounded channels backpressure rather than drop; select fairness is acceptable before receiver closure; theme blends match the sim values; transliteration-by-default is a reasoned and documented terminal-safe deviation; `run --jsonl` behavior is untouched.

Verification: all 132 locked workspace tests passed, formatting/diff checks passed, and `xtask check` reported 132 tests with no LOC violations. Clippy could not acquire the read-only target lock, so no clippy result is claimed. Worktree remained clean.

VERDICT: NO_SHIP
hook: Stop
hook: Stop Completed
tokens used
241,438
NO_SHIP. Two P1 defects affect the acceptance path and terminal restoration.

## Findings

- **P1 — Demo completion causes a permanent hot loop.** `runtime.rs:115` keeps polling the closed envelope receiver; `None` repeatedly sends `StreamEnded`, and `app.rs:92` marks every no-op dirty. The loop spins while redrawing at 30 fps. A PTY run consumed 12.68s user CPU in 20.04s. Fuse/disable the receiver branch after the first `None`.

- **P1 — Terminal setup is not transactional.** `runtime.rs:31` enables raw mode before `runtime.rs:32` can fail entering alternate screen or enabling paste. That error returns before a guard exists, leaving raw mode—or a partially entered alternate screen—active. Establish rollback immediately after enabling raw mode.

- **P2 — Raw projection ignores `RenderTargets.ui`.** `projection.rs:137` applies every payload regardless of `envelope.render.ui`. Events explicitly marked non-UI can mutate badges, transcript, menus, and usage. Sequence accounting should advance, but display mutation must be skipped when `ui == false`.

- **P2 — Completed item IDs are not idempotent.** `projection.rs:233` replaces only a still-streaming block; a later `Completed` for the same ID appends a duplicate at line 241. Plans similarly append every all-done completion at `projection.rs:225`, while a later `Started` can repin a closed ID. Exact duplicate sequence numbers are skipped, but lifecycle duplication under a new sequence violates replace semantics.

- **P2 — Blocking menus are invisible and unanswerable in the live TUI.** `render.rs:160` always renders the composer and never consults `open_menu()`. `app.rs:120` also treats Enter as composer input and exposes no answer side effect. A `MenuOpened` run can therefore block with no usable interaction, unlike the sim’s composer-replacement menu.

- **P2 — Command-output honesty is not surfaced.** `render.rs:277` ignores both `output_truncated` and `output_decode_error`. The plain renderer reports truncation but still ignores decode failure at `plain.rs:117`. Invalid chunks are silently omitted, allowing incomplete output to appear authoritative.

- **P2 — Plain-output write failures panic instead of returning a defined exit code.** `main.rs:117` uses `print!`, which panics on BrokenPipe. Reproduced with a closed pipe. Use fallible stdout writing and map the result deliberately—normally BrokenPipe success or `EX_IOERR`.

- **P3 — The terminal guard is not nestable and permanently stacks panic hooks.** `runtime.rs:33` installs a hook that Drop never restores. Re-entry nests hooks, and dropping an inner guard leaves an outer guard believing the alternate screen is still active. This is outside the one-shot CLI path but conflicts with the public guard API.
