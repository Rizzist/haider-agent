# Lane winclip — highlight/copy/paste must work on Windows (v0.0.970, gpt-5.6 xhigh)
Worktree lane-970-winclip (from origin/wave-970). OWNER (2026-09-04): "fix windows highlight copy and paste (it doesn't work)".
CLAIM-AUDIT FIRST, then fix. Known ground truth in this tree (verify every line before trusting it):
- The TUI enables mouse capture for the whole session (crates/haider-tui/src/runtime.rs:545 EnableMouseCapture, disabled at :583). While a
  terminal is in mouse-reporting mode the terminal's OWN click-drag selection is suppressed, so "highlight with the mouse and press
  Ctrl+C / right-click" does not reach the system clipboard. On Windows Terminal / conhost the Shift-bypass that macOS users rely on is not
  the same gesture, so Windows users lose highlight-copy entirely.
- The copy-out path is macOS-shaped: crates/haider-tui/src/clipboard.rs spawns `pbcopy` as the authoritative local clipboard and emits
  OSC 52 as best effort. There is no Windows local-clipboard writer; a Windows user gets OSC 52 only, which conhost and many terminals
  ignore or truncate (OSC 52 payload limits), so copies silently do not land.
- The read side (Ctrl+V image/text paste) landed in v0.0.970 via `arboard` with the clipboard-win backend, but it was NEVER EXECUTED on
  Windows (composerfix lane, "reasoned about, not executed") — verify it actually works, including CRLF and UTF-16 text.
- In-app selection exists (runtime.rs:2963 rendered_selection_text, :3123 copy_selection_effects) — determine what gesture reaches it on
  Windows today and whether it covers transcript text, not just the composer.
Deliver:
1. Local clipboard WRITE on Windows: use `arboard` (already a dependency; clipboard-win backend, no new crate if avoidable) for
   SetClipboardData with CF_UNICODETEXT, replacing the "pbcopy or nothing" fallback with a per-platform writer (macOS pbcopy, Windows
   arboard, Linux existing path); keep OSC 52 as the remote/best-effort layer exactly as documented. The copy confirmation flash must tell
   the truth per platform (confirmed local copy vs OSC-52-only).
2. Highlight/selection that works under mouse capture: in-app mouse drag over the transcript selects text (visible highlight), and
   Ctrl+Shift+C (Windows convention) plus the existing copy gesture copy the selection; document and support the terminal-passthrough
   escape too (Shift+drag on Windows Terminal) by NOT swallowing shift-modified mouse events, so the native terminal selection still works
   for users who prefer it. Right-click paste (Windows Terminal convention) must not be eaten by the app.
3. Verify the READ path on Windows (text + image), CRLF normalization on paste, and UTF-16 round-trip; fix what is broken.
4. If any part cannot be executed on this machine (macOS), say so explicitly and gate it behind the Windows CI job rather than claiming it
   works: add/extend a Windows CI test that exercises the clipboard writer and the paste path headlessly (no interactive terminal).
Tests: unit tests for the per-platform writer selection and the truthful flash; selection-model tests (drag start/extend/clear, multi-line,
wrapped rows); key/mouse routing tests proving shift-modified mouse events are passed through and right-click is not swallowed; CRLF/UTF-16
paste tests; goldens only where a row visibly changes (state why). Run `cargo test --workspace` and `cargo clippy --workspace --tests --
-D warnings` with the ENV LAW, update test-baseline.txt via the repo's test-count tool. Write docs/testing/v0.0.970/winclip.md (what was
executed vs inspected, per platform). Commit on the lane branch, no co-author trailer, do not push. LAST line: SHIP or NO_SHIP.
