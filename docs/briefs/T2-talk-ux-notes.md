# T2 — TUI talk UX (`/talk`, the wave, the ghost row, setup) — implementation notes

Implementer: Fable 5. Branch `t2-talk-ux`, built on `main` @ `867801a`
(T1 merged). Scope contract: `docs/briefs/T-wave-talk-transcription-brief.md`
(T2 + locked decisions); mechanics authority:
`docs/research/t-wave-transcription-research.md` Part C; crate surface:
`docs/briefs/T1-stt-engine-notes.md`. `haider-stt` finally has its consumer
edge — nothing engine-side was reimplemented, only consumed.

## Shape

Two new TUI modules with a hard determinism boundary between them:

- `crates/haider-tui/src/talk.rs` — EVERYTHING deterministic: the
  toggle-to-talk state machine data (`TalkState`, `TalkPhase`,
  generations), the wave ring + glyph/smoothing/calibration math, the
  ghost assembly (local cumulative / Deepgram finals+interim), the setup
  card (`TalkSetupCard` — the LoginCard modality), and the runtime
  vocabulary (`TalkShellCommand` out, `TalkEvent` back).
- `crates/haider-tui/src/stt_runtime.rs` — the ONE place that touches
  cpal/whisper-cli/Deepgram/downloads/config IO: a single supervisor task
  (the Link pattern) owning the live capture session. Kept logic-free on
  purpose; every decision lives in `talk.rs` + the reducer, so the
  law-untestable seam stays thin (T1's `CaptureWorker` discipline,
  extended one level up).

Reducer glue lives on `AppModel` (`talk_toggle`, `talk_key`,
`handle_talk`, `talk_secret_*`, `talk_setup_*`) — the same pattern as the
login card. Runtime wiring: `run_live` spawns `TalkRuntime`, loads the
profile `transcription` section once at boot (typed error kept, never
defaulted), and gains ONE select branch for talk events. `ShellRequest`
gained `Talk(TalkShellCommand)`; `AppRequest` gained
`TalkShell` / `TranscriptionSecretRead` / `TranscriptionSecretStore`.

## 1. `/talk` + TalkChip live wiring

The research doc's `app.rs:7524-7534` refusal arm (relocated post-F2 to
the `Hit::TalkChip` arm, was `app.rs:7715-7726`) now drives the REAL
machine in live mode; demo keeps the canned sim hold untouched. State
machine: `Idle → Starting → Listening → Finishing → Idle`, one
generation mint per start AND per settle, so every late runtime event
(envelope, partial, `Finished`) correlates against a dead generation and
drops whole. Gestures while engaged:

- **Esc** cancels — discard by contract; nothing lands anywhere.
- **⏎** commits + submits: input stops, the ENGINE's assembled result
  (never the ghost) realizes into the composer and rides
  `submit_composer()` — the one submit path, so steer/queue/free-text-menu
  semantics apply unchanged.
- **typing/paste** commits the GHOST into the composer (one separating
  space, capped at the ADE 8k insert cap) and the key/paste then flows
  the NORMAL path (`talk_key` returns false) — the engine's unseen tail
  is discarded: what you saw is what you keep.
- everything else is inert; ⌃C stays the navigation hatch (the stash
  seam cancels the session — talk is surface-local exactly like the
  login card, torn down in `stash_draft`).
- chip press while listening = the ⏎ gesture (toggle-to-talk).
- the 900 s capture cap finishes with `CommitIntent::Insert` — realized,
  flashed, never auto-submitted.

## 2. The right-to-left wave

`WaveRing`: fixed `WAVE_WIDTH = 24` slots; the newest envelope sample
enters at the RIGHT edge, history flows left (one slot per ~60 ms
capture emission ≈ the last 1.4 s). Pipeline per sample: session noise
floor (running minimum since activation; headroom-rescaled with a 0.05
denominator floor) → asymmetric smoothing (attack 0.5 / decay 0.13, the
ADE voice-ring recipe) → ring. Render: perceptual `sqrt` → 8-level
partial blocks `▁▂▃▄▅▆▇█`; per-column ink split at 0.12 — gold while
speaking, faint history (THEME SLOTS ONLY; no new slots, so the WCAG law
needed no extension). `/talk wave` flips a plain-ASCII ramp
(`_.:-=+#@`) for fonts without partial blocks. Placement: the composer
band's first row, directly left of the TalkChip; on a band too narrow
for both, the wave yields and the chip keeps its fit. While engaged the
long session placeholder gives way to the short gesture contract
(`speak — ⏎ send · esc cancel`) so the wave has its room. Repaints ride
envelope-event dirty marks through the EXISTING guarded 33 ms frame tick
— zero new timers; `animated()` already covers `listening` for the chip
pulse.

## 3. The ghost row

One dim row carved at the very top of the composer band (`◉ <text>`,
tail-kept behind a leading `…` when overlong), replaced per partial:
local whisper's cumulative text verbatim; Deepgram as joined finals +
latest interim. Realized into the composer only on commit. CHROME BY
LAW: the predicate is shared between `composer_height` and the paint (the
B4b attachment-row discipline), and nothing ever touches the projection —
pinned by `the_ghost_row_is_chrome_never_content` (entry count identical
across a 12-partial dictation), so F2's markdown line-stability holds by
construction.

## 4. The setup surface

`/talk setup` (and `/talk` when unconfigured — a Deepgram start with no
vaulted key, or a local start hitting `ModelMissing`/`RuntimeMissing`,
lands here automatically with the honest reason). The card owns the
input band + keyboard exactly like the login card: the key never reaches
the composer/palette/ring, renders as a capped mask, `Debug` redacted,
zeroize-on-drop, TAKE-semantics (one live copy between card and wire).
Stages: engine picker → local (3 catalog rows with
installed/download-%/failed states + the whisper-cli row with per-OS
install driver) → deepgram key (paste → `/v1/auth/token` validate +
streaming-model fetch in one probe → vault via daemon → models → language
with client-side charset check). A vaulted key offers `⏎ reuse · r
retype`; reuse re-validates via the vault read and never re-stores.
Config saves are never optimistic — the card closes on `ConfigStored`
only, and a PRESENT-but-corrupt profile section surfaces typed at
`/talk` (never silently defaulted — T1's config law carried to the UI).
Honest errors: mic denied (TCC hint enriched with the actual
`TERM_PROGRAM` app name), model missing/evicted (reinstall row), key
invalid (401 wording), endpoint/transport failures verbatim.

## 5. Wire + client plumbing

`haider-client` gained `transcription` — the thin helpers T1 left:
pure builders (`secret_get_request`/`secret_set_request`) and parsers
(`secret_from_get_response`/`present_from_set_response`) CONSUMING
`haider-rpc`'s frozen bodies, plus async `secret_get`/`secret_set` over
`RpcClient`. The TUI link's `request_body`/`map_response` delegate to the
SAME builders/parsers (one authority; pinned by
`the_link_and_the_client_helpers_agree`). New `LiveCommand`s
(`TranscriptionSecretGet`/`Set`) carry NO durable id — the set is
deliberately receipt-free (no receipt may carry a secret; T1 daemon law)
and never outboxed; errors come back op-tagged through the link's
request context (the hooks-list precedent). `LiveReply` gained
`TranscriptionSecret`/`Stored`/`Failed`, applied as pure reducer routing.
The reducer feature-gates on `transcription_v1` before any secret RPC.
NO wire shapes were added or changed — goldens untouched.

## 6. cpal glue (the thin seam)

`TalkRuntime::spawn` starts the supervisor; per session it: spawns T1's
`CaptureWorker` on the blocking pool (enriching a `MicUnavailable` hint
with the terminal-app name), opens the engine at the mic's native rate
(local: dir-resolve → `effective_model` → installed check → runtime
discovery → `start_partial_session`; deepgram:
`DeepgramSessionConfig::new` + `start_session`), bridges the worker's
std-mpsc events on a dedicated thread (the stdin-reader pattern —
envelopes/health/cap → `TalkEvent`s, PCM frames → the engine forwarder),
and tears down mic-first on finish (frames end, then the engine flushes).
Cancel cancels the local token / drops the Deepgram sender (its
CloseStream drain self-terminates). One-shot jobs (probe, download with
progress, runtime install, snapshot, config save) are independent tasks
so a 900 s model download never blocks a talk start.

## Laws added (ledger 1783 → 1846, +63)

- `t2_wave_tests` (11): ring fixed-width/newest-right/history-shift,
  attack=0.5 and decay=0.13 pins, noise-floor flatten+rescale, sqrt
  mapping total/monotone/endpoint pins, plain-ASCII fallback indices,
  hot-threshold split, band render (24 glyphs left of the chip, decay
  toward the left edge), narrow-band yield, stale-generation envelope
  drop, `/talk wave` round-trip toggle.
- `t2_talk_state_tests` (23): chip/slash start, demo refusal, Started
  generation gate, Esc-discards, Enter-commits-engine-result-and-submits,
  typing-commits-and-keeps-editing, paste-commits, inert-other-keys,
  late-Finished drop, cap-no-autosubmit, finish-error keeps watched
  words, surface-change cancel, local-cumulative + Deepgram
  finals/interim assembly, ghost chrome-not-content, Deepgram
  key-read-first + spec mapping, no-key→setup, feature gate, ModelMissing
  →reinstall surface, TCC hint, secret-read-failure settles, realize cap.
- `t2_talk_setup_tests` (15): open+loads, snapshot→catalog rows, install
  select via store (no optimistic close), download progress/finish/fail,
  runtime install drive, the full key flow (validate BEFORE vault, one
  live copy), 401 honesty, reuse-skips-store, retype, feature refusal at
  the picker, language validation, Esc close, Debug redaction (card AND
  whole-model), corrupt-config surfacing.
- `t2_talk_link_tests` (6): request bodies vs the T1 goldens, response→
  reply mapping, op-tagged errors, no-durable-id pins, driver routing
  (reads never outboxed; TalkShell never a wire command), link≡helper
  agreement.
- `haider-client/tests/transcription_tests` (4): golden builders, golden
  response parsing (absent field → None), typed refusal vs skewed-body
  distinction, full async round-trip against a fake UDS daemon
  (set→get→clear).
- Updated pins (registry extensions, intent preserved):
  `hit_alignment_tests` ×2 (the `/t` palette now has 5 rows),
  `sim_parity_r2_tests` wrap-around (5 rows), and
  `w3c31_r2_tests::live_push_to_talk_never_wedges_listening` — the old
  live-refusal pin is SUPERSEDED by T2; its intent (no un-clearable
  hold) survives verbatim: press arms with a runtime effect behind it,
  the driver mints no wire command, Esc clears.

## Verification

- `cargo fmt --all` clean; `cargo clippy --workspace --all-targets` zero
  warnings; `cargo test -p haider-tui -p haider-client -p haider-stt`
  green (ulimit 8192, CARGO_INCREMENTAL=0); full workspace suite green.
- `scripts/tui-probes/ladder.sh`: 16/16 PASS (14 demo + 2 live).
- `xtask test-count --update`: baseline 1783 → 1846.
- Mutation campaign: 10 EXECUTED kills — see
  `T2-talk-ux-mutation-notes.md`.

## Honest deviations / notes

1. **The wave style flag is session-local, not persisted** — `/talk
   wave` flips it per run. `tui-settings.json` is a strict
   version-1/theme-only record; widening it is display-settings scope a
   later polish lane can take with its own migration law.
2. **Enter during `Starting` is an inert no-op** (nothing captured yet);
   a second chip press or Esc cancels. The brief specifies gestures for
   LISTENING; this is the conservative reading.
3. **`VoiceState`'s demo default (`whisper-large-v3`) is untouched** —
   it is sim-parity scaffolding for the demo `/voice` card; the live
   truth is the profile `transcription` section. The research doc's
   "realign that default" note is deliberately not taken this wave.
4. **`talk_secret_failed` on a `Set` error while the setup card is
   closed** falls back to a flash — the card may have died to a surface
   change mid-store; the vault file is the durable truth either way.
5. **The stt supervisor's `Finish` drops the capture worker before the
   engine flush** — a deliberate ordering (frames end, tail chunk
   flushes). The cpal seam itself (device open, stream callbacks,
   teardown joins) remains law-free by construction, like T1's
   `CaptureWorker`.

## What needs the live-mic ship-gate probe

Everything below the deterministic boundary, on this Mac, per the
brief's Ship section:

1. Real `/talk` with the shared `ggml-base.en.bin` already on disk —
   verify NO re-download (T1's short-circuit law, live), whisper-cli
   discovery against the Homebrew install, and end-to-end partials →
   commit → submit.
2. The TCC prompt naming the terminal app; a DENIED mic showing the
   enriched hint (digital-zero watchdog path).
3. The Deepgram path with a real key (paste → validate → vault → model
   list → dictation), including the KeepAlive/CloseStream lifecycle
   against the live endpoint.
4. Wave feel under both themes (gold/faint contrast, decay tail) and
   under `/talk wave` plain glyphs; the ~30 fps ride on real envelope
   cadence.
5. The 900 s cap and a mid-session `uninstall` from the ADE (eviction →
   `ModelMissing` reinstall surface) — evictability under fire.
