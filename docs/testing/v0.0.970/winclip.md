# v0.0.970 Windows clipboard lane

## Verdict and execution boundary

**NO_SHIP.** The local clipboard and application input changes are implemented,
but native Windows terminal-owned paste has a confirmed dependency-level gap by
source inspection. This macOS machine cannot execute the Windows clipboard or
ConPTY path. The macOS workspace tests and Clippy passed. The new Windows CI test is mandatory native clipboard evidence;
it is not evidence for terminal input parsing. Git metadata writes are also
sandbox-blocked, so the requested merge-forward and commit cannot be recorded.
No push was attempted. Final staging of only the ten implementation/test/report
files failed creating `index.lock` with `Operation not permitted`; consequently
`git commit` could not run and no commit or co-author trailer was created.
The supplied lane briefs and turnperf evidence were not included in staging.

The audit started at `b9c2a0475214102d1fb4c8d9c3ae3f480fd05fe4`, which is also the
local `origin/wave-970`. The required fetch failed opening `FETCH_HEAD`; the
fallback `git merge --no-commit origin/wave-970` failed creating `ORIG_HEAD.lock`.
Both errors were `Operation not permitted` in the parent repository's Git
metadata. During verification the shared `origin/wave-970` ref advanced to
`2ef44708757e0f87b4437ec4ab1594c6a680814e`; this lane remains at `b9c2a047`.
The final tests therefore cover this lane tree, not the newer unmerged tip.
No conflicts or merged prompt changes were introduced. The instruct
pipe byte pin and provider-request goldens therefore need no regeneration.

Read first: the supplied `LANE-COMMON.md`, `LANE-BRIEF-winclip.md`, and turnperf /
turnperf2 evidence. The latter are turn-latency/durability research, not evidence
of Windows clipboard execution. Those supplied files are excluded from changes.

## Claim audit before implementation

| Supplied claim | Audit | Evidence in the starting tree |
| --- | --- | --- |
| Mouse capture for the session, runtime 545 / 583 | Correct, exact lines | `TerminalGuard::enter` enables capture; restoration disables it. |
| Windows has no comparable Shift bypass | Wrong for Windows Terminal; conhost behavior not executed | Microsoft's documentation explicitly supports Shift+drag during mouse mode. |
| Copy is pbcopy plus OSC 52, with no native Windows writer | Correct | `clipboard.rs:37` unconditionally spawns `pbcopy`; `runtime.rs:3123` invokes it then emits OSC 52. |
| arboard text/image read landed; Windows execution never happened | Backend correct / unexecuted confirmed; text handling incomplete | arboard 3.6.1 does use clipboard-win on Windows. `ClipboardContent::Text` was a marker; the decoded string was discarded and the runtime did nothing for it. Forwarded Ctrl+V could not paste text. |
| Selection exists at runtime 2963 / 3123 | Correct, exact lines | Unmodified left Down/Drag/Up already selects rendered screen cells, paints a highlight, and auto-copies on release. This includes transcript text and wrapped rows, not just composer text. |
| Ctrl+Shift+C copies the transcript | Missing | Any key cleared the transcript selection; only the composer had a Ctrl+C gate, matching lowercase `c`. |
| Shift-modified mouse and right-click are preserved | Missing app routing | Mouse modifiers were not inspected. Right-button reports fell into a no-op. Native terminal-owned gestures cannot be replayed after the terminal has delivered an app event. |
| CRLF normalization needs adding | Already present | Small paste and large paste-pill insertion both normalize CRLF and lone CR to LF. Tests now cover the actual clipboard-to-paste path too. |

[Microsoft selection documentation](https://learn.microsoft.com/en-us/windows/terminal/selection)
confirms Shift+drag during mouse mode and right-click copy/paste behavior.

## Implemented behavior and gestures

- On Windows, `copy_local` uses existing arboard `set_text`. The pinned Windows
  backend writes CF_UNICODETEXT through clipboard-win / SetClipboardData. No new
  crate, feature, or unsafe code was added. macOS retains pbcopy; Linux retains
  the existing pbcopy attempt (normally unavailable, then OSC 52 only).
- OSC 52 is always attempted after the local writer as the documented remote /
  embedded-terminal mirror. `· copied` requires local confirmation. A successful
  output write alone says `· copy unconfirmed — sent via OSC 52 only`; failure
  of both output paths reports failure. Terminal acceptance of OSC 52 is never
  inferred from writing the sequence.
- Unmodified left-drag over the transcript visibly selects screen cells;
  release copies. Ctrl+Shift+C re-copies a transcript selection, accepts both
  `c` and `C` event spellings, and also copies a composer selection. With no
  selection it does nothing. Existing composer Ctrl+C and release-to-copy
  behavior are retained; bare Ctrl+C with only a finished transcript highlight
  retains its navigation meaning.
- Hold Shift while dragging in Windows Terminal to use its native selection.
  Shift-modified reports that reach the app do not click, scroll, select, paste,
  or update its stored hover pointer. They cancel any pending app drag. This
  is an app routing guarantee, not a claim that the app can replay reports
  into the terminal. Conhost's native selection gesture is unexecuted here.
- If the terminal delivers a captured right-button press, the app requests one
  clipboard paste. Right-button drag/release do not repeat the paste. When the
  terminal owns a right-click, its resulting text follows terminal ingress;
  see the Windows dependency blocker below.
- A forwarded Ctrl+V / Ctrl+Shift+V / Command+V reads both text and images.
  Text retains a zeroizing, redacted `Pasted` payload and enters the same paste
  reducer as terminal text. Image capability, daemon-store, and attachment-count
  gates apply only after reading an image. Text remains available without them.
  Editable dialogs use their existing masked/field paste target. Read-only
  screens and modal-hidden composer selections cannot access the parked draft.
  Loom retains its existing Ctrl+V validation shortcut.

## Native Windows input blocker

Pinned crossterm 0.29 has a Windows event source based on native INPUT_RECORDs.
It constructs key/mouse/resize/focus events, never `Event::Paste`.
`EnableBracketedPaste` emits `CSI ?2004h`, but Windows `EnableMouseCapture` sets
console mode to `0x0098`, excluding `ENABLE_VIRTUAL_TERMINAL_INPUT` (`0x0200`).
Windows Terminal wraps paste with CSI 200~/201~. ConPTY's input parser, with VT
input disabled, discards those unsupported generic-key delimiters. Payload is
then delivered as ordinary key events, including Enter: multiline paste can
submit instead of insert. A coalescer over already-decoded crossterm events
cannot recover the missing boundaries.

This is **inspected, not executed**. Primary evidence:
[Microsoft console modes](https://learn.microsoft.com/en-us/windows/console/setconsolemode),
[Windows Terminal PasteText](https://github.com/microsoft/terminal/blob/main/src/cascadia/TerminalControl/ControlCore.cpp),
[ConPTY input dispatch](https://github.com/microsoft/terminal/blob/main/src/terminal/parser/InputStateMachineEngine.cpp),
[crossterm issue 737](https://github.com/crossterm-rs/crossterm/issues/737), and
[the proposed upstream VT/native input repair](https://github.com/crossterm-rs/crossterm/pull/1030).
The locally installed locked crossterm source corroborates the event-source and
mode-setting facts.

The Windows CI clipboard test exercises actual OS reads and writes plus the
production forwarded-key reducer; it cannot certify intercepted Windows Terminal
Ctrl+V, terminal-owned right-click, empty/image paste, or conhost interaction.
Closing this blocker requires a Windows input transport repair and a headless
ConPTY test through production terminal initialization that injects a bracketed
multiline Unicode paste and proves one paste, normalized text, and no submit.
No heuristic timing-based interception of Enter was added.

## Tests and gates

All build/test commands use:

```sh
RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 \
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
```

`df -m /` was checked before builds; the 700 MiB stop threshold applies.
The first workspace test compilation was stopped before the disk floor as
concurrent work reduced available space. Only this lane's generated test
executables were removed (116 files, 4,817 MiB of path sizes); sources and
assertions were untouched. The complete rerun adds
`CARGO_PROFILE_TEST_STRIP=symbols` to reduce generated executable size and
`--test-threads=4` to bound runtime concurrency. This changes symbol retention,
not test selection or assertions. The independent test-count tool used a small
separate target directory to avoid waiting for the workspace Cargo lock.

CLI and daemon siblings were prebuilt, then tests set
`HAIDER_TEST_SIBLINGS_PREBUILT=1`. The initial built daemon was 198,060,144 bytes,
well over 10 MiB.

| Platform / gate | Evidence |
| --- | --- |
| macOS portable routing, clipboard policy, paste model, existing selection/composer tests | 50 tests PASS across four targeted binaries (14 selection, 21 composer/image, 4 clipboard policy, 11 input). |
| Windows native OS clipboard | NOT EXECUTED locally. New `w970_winclip_windows_tests` runs normally and explicitly in the Windows xplat test job, without ignore/env opt-out. |
| Windows native terminal delivery and mouse interaction | INSPECTED only; blocker above. |
| Linux writer/read desktop behavior | INSPECTED only; existing fallback preserved. |
| Workspace tests | PASS: `cargo test --workspace --locked --no-fail-fast -- --test-threads=4`, ENV LAW plus test-symbol stripping; 5,199 passed, 0 failed, 13 existing ignored. Cargo exit 0, including doc-tests. |
| Workspace Clippy tests with `-D warnings` | PASS: `cargo clippy --workspace --tests --locked -- -D warnings` with ENV LAW; completed in 3m 59s. |
| fmt / unsafe-count / baseline | PASS: `cargo fmt --all --check`, `scripts/check-unsafe-counts.sh` (production=189, test=20); repository `xtask test-count --update` changed 4,788 → 4,805. |

Named regression coverage:

- `w970_winclip_clipboard_tests`: platform writer policy, truthful three-outcome
  flash, redacted/zeroizing text, exact UTF-8 OSC 52 mirror.
- `w970_winclip_input_tests`: real rendered wrapped transcript drag/extend/
  release/highlight/clear, both copy-key cases, composer copy, terminal-owned
  Shift events, Shift handoff, one captured right-click paste, single terminal
  paste, CRLF/lone-CR/UTF-16 preservation for short text and large pills,
  hidden-draft isolation, masked login paste, and talk key/language fields.
- `w970_composerfix_tests`: previous image/notice/layout assertions retained;
  tests now execute the image read before asserting image-specific refusals.
  The former text-no-op and pre-read-vision expectations were corrected to
  the requested text-paste behavior. Added text paste across no-vision,
  missing-artifact, full-attachment, and demo states; image refusals remain.
- `w970_winclip_windows_tests`: one serialized native test exercises real
  CF_UNICODETEXT round-trip with supplementary Unicode, CRLF/lone CR, >128 KiB
  text, real 2x2 RGBA image read/PNG/upload, and empty clipboard. Missing native
  clipboard access fails the job. No interactive terminal is needed.

No golden was changed: no static row layout changed. Existing highlight pixels
and wrapped text are checked from actual ratatui frames. Image notice row
assertions remain intact; only their setup now performs the clipboard read.

## Independent verifier

Two findings, both changed code and tests: (1) clipboard gestures reached a
hidden composer on read-only/modal surfaces; explicit visible-target gating
prevents this; (2) forwarded/modal paste was dropped or typed a literal `v`;
paste is now routed before modal key handling through the protected paste path.
No findings rejected as noise. Verifier verdict remains NO_SHIP because the
Windows terminal-delivery blocker and native execution gap are still explicit.

## CI error registry walk

Walk uses the checked-in registry class descriptions in
`docs/testing/v0.0.968/qafix-ci-registry.md` and the #96 extension in
`scripts/qa-gate/CI_REGISTRY_WALK_QAGATE3.md`. "Unchanged" means no affected
surface was introduced, not that a foreign-platform behavior was executed.

| Classes | Disposition and evidence |
| --- | --- |
| 1–4, 6, 8, 12–14, 35–36, 39, 48, 62 | Typed clipboard text and explicit request/image gates; production routing exercised through public seams; new tests live in separate files. Workspace compiler/Clippy results recorded above. |
| 5, 27, 37, 50, 55 | Windows behavior explicitly isolated from host execution claims. Platform writer policy tests run on macOS; native Windows test is mandatory CI. No platform-dependent golden changed. |
| 7, 24, 31, 34 | Dependencies/features/catalog/Android unchanged; existing locked arboard supplies the native backend. |
| 9–11, 15–19 | Standard formatting and Clippy gates; no production unwrap/expect, unsafe block, async lock, or new lint suppression. Test-only expect allowance follows repository convention. |
| 20 | Baseline updated only with the repository `xtask test-count --update`; no deleted/ignored regression tests. |
| 21, 54, 67, 72, 74 | ENV LAW applied; explicit prebuilt CLI/daemon siblings and deterministic test device/discovery. Native clipboard mutations occur only in the Windows test binary, never in the portable policy/model tests. |
| 22, 26, 40, 63, 68 | OS clipboard ownership is intentional. Read errors retain a notice instead of becoming false empty; local copy confirms only writer success; existing pbcopy process path preserved. |
| 25, 52, 57, 59 | No render performance or layout change. Real wrapped transcript/highlight buffers and prior notice-row assertions are checked without rebaselining goldens. |
| 28–30, 41–44, 46–47, 49, 51, 53, 56, 58, 60–61 | Process, socket, daemon lifecycle, error terminal, CAS, and account/store authority are unchanged. Full workspace checks exercise their existing suites; unrelated failures must remain attributed separately. |
| 32–33, 45, 64–66, 69–71, 73, 76–78 | No release, push, unsafe addition, STT engine, source-window pin, archive, or executable-name change. Explicit xplat native clipboard test step preserves the existing full test runner. Built daemon exceeds 10 MiB. Commit/merge attempts are reported honestly rather than inferred successful. |
| 23, 38, 75, 79–93 | No schema, map-key, supervisor, maintenance, sparse-file, process completion, publication, or shutdown change. |
| 94–95 | No new deadline/wait or retained-connection external-state wait. Existing pbcopy confirmation poll retained; Windows occupied-clipboard retries are the pinned dependency's five 5 ms retries (25 ms sleep budget). |
| 96 | No latency claim, benchmark change, or durability optimization. Supplied turnperf evidence is not reused as clipboard proof. |

No new registry class is asserted. The native terminal-input limitation is a
known upstream correctness gap and remains a shipping blocker.
