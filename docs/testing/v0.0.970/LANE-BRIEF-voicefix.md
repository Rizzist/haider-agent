# Lane voicefix — voice input QoL: no auto-send, real-time level bars, steady blink, half-width visualizer, smooth animation (v0.0.970, Opus UI lane)
Worktree lane-970-voicefix (from origin/wave-970, AFTER composerfix lands — both touch the composer/render). OWNER (2026-09-03, screenshots):
Whisper transcription works. Required changes:
1. NO AUTO-SEND: when the user stops listening (presses the listening toggle / "talk" again), the transcribed text is inserted into the composer (appended
   at the cursor, editable) and NOT sent; the user sends with Enter as usual. A one-line notice ("transcribed — edit and send") is fine. Keep a way to opt
   into auto-send only via an explicit setting (default off).
2. LEVEL BARS IN REAL TIME: the golden audio-level bars must update live while the user speaks (like the tps widget: sample the mic level at ~20–30 Hz,
   EMA-smoothed, decay when silent) — today they appear frozen/late. The "◉ listening…" indicator blinks at its OWN steady rate (~1 Hz), independent of the
   bar updates (do not couple the blink to audio frames).
3. ANIMATION COST: bar/blink updates must not slow the TUI: coalesce redraws (render at most ~30 fps while listening, only the affected rows if the
   renderer supports partial invalidation), no allocation per audio frame, no full-transcript re-layout on a level tick (tuivirt keeps the viewport cache;
   make sure the audio widget invalidates only itself). Measure: frame time during listening before/after (report p50/p95), CPU of the TUI while listening.
4. WIDTH: the visualizer is too wide — halve its width (fixed cell budget, left part of the status row), keeping alignment with the model/mode segment on the right.
Tests: unit tests for the level estimator (steady tone, bursts, silence decay), the no-auto-send state machine (stop listening → composer text, no submit),
blink cadence independent of level ticks; golden frames of the listening row (idle / speaking / stopped) at 80/118/160; existing voice/dictation tests
green (crates/haider-tui: mic engine resilience, STT surfaces); tuivirt/tpsfix/composerfix goldens green except the rows this changes. `cargo test -p
haider-tui`, clippy -D warnings, test-count update. Commit on the lane branch (no co-author trailers); report frame-time numbers. LAST line: SHIP or NO_SHIP.

CODE POINTERS: see crates/haider-tui/src for the voice/dictation/mic modules (listed by the orchestrator at launch) and the "talk" toggle in the composer/status row.
