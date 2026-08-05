# T-wave — talk/dictation: shared local Whisper + Deepgram cloud

Authority: `docs/research/t-wave-transcription-research.md` (implementation-grade,
code-cited against the Diff Forge ADE and current Deepgram docs). This brief is
the scope contract; the research doc wins on mechanics.

## Owner contract (verbatim)

"look at how Diff Forge AI ADE handles instaling whisper local or allowing
Deepgram API, we should support this *independently* from the main Diff Forge
AI ADE (but install location should be same, so we can use the same
transcription model but independently though the Harness, make sure talk is
fully functional (and cool UI for audio input from right to left, ascii waves
as user speaks over time) - also it should support accepting deepgram api key
then fetching models, selecting, for cloud transcription instead of local.
make transcription fully supported."

`/voice` (GPT-Realtime-2 or STT-LLM-TTS) is explicitly LATER. Not this wave.

## Locked decisions

1. **Process placement**: audio capture + both STT engines live in the TUI
   process (new crate `haider-stt`, consumed by `haider-tui`). macOS TCC
   attributes mic grants to the responsible app; the detached daemon is the
   fragile place. The daemon's ONLY role: vault the Deepgram key.
2. **No whisper linking**: reproduce the ADE's whisper.cpp **CLI-spawn**
   pattern (discovery order + brew-driven install on macOS, pinned v1.8.4
   zip+sha256 on Windows, PATH-hint on Linux). No FFI, no unsafe, release
   binary unchanged.
3. **Byte-for-byte shared model dir**: implement the ADE resolver verbatim
   (`$RUST_DIFFFORGE_DATA_DIR` → `$RUST_DIFFFORGE_HOME` → macOS
   `~/Library/Application Support/DiffForge` / Windows `%APPDATA%\DiffForge` /
   Linux `$XDG_DATA_HOME/diffforge`|`~/.local/share/diffforge` — lowercase on
   Linux) + `/whisper`. Same filenames, same sha256 catalog, same
   `.download` → verify → atomic-rename. Read `selected-model.txt` as a
   default HINT; **never write it** (never flip the ADE's selection). Models
   are evictable at any moment (the ADE's uninstall deletes the whole dir) —
   "model missing → reinstall?" is a first-class state, never a crash.
4. **Deepgram key is vaulted, not localStorage**: `FEATURE_TRANSCRIPTION_V1` +
   `transcription.secret_get`/`transcription.secret_set` (UDS-only), riding
   the existing SecretWire + FileVault machinery. Never logged, ≤512 chars,
   no control bytes. Wire goldens regenerated.
5. **Deepgram surface** (all doc-verified): auth `Authorization: Token <key>`;
   paste-time validation `GET /v1/auth/token`; model catalog
   `GET /v1/models` filtered `streaming: true` (excludes batch-only
   `whisper-*`; Flux excluded — it lives on `/v2/listen`); streaming
   `wss://api.deepgram.com/v1/listen?model=<sel>&language=<l>&encoding=linear16&sample_rate=<native>&channels=1&interim_results=true&smart_format=true`;
   binary i16 LE PCM frames; `{"type":"KeepAlive"}` every 3–5 s;
   `{"type":"CloseStream"}` + bounded drain to end. Hard session cap
   (ADE precedent: 900 s capture).
6. **Local engine mechanics**: ADE-parity — 3-model catalog
   (tiny.en/base.en/small.en with the exact HF URLs + sha256 from the
   research doc), 16 kHz mono WAV chunks, whisper-cli args
   `-m <model> -f <wav> -l en -t <threads 4–8> -nt -np -bo 1 -bs 1 -nf`,
   pseudo-streaming chunker (silence ≥750 ms once ≥10 s buffered, force at
   35 s), hallucination policy (drop <900 ms / RMS <0.01 / bracketed
   markers / low-energy "you"), stderr warm-up noise filtered, per-chunk
   spawn with warm page-cache pre-read, cancel tokens.
7. **Engine selection is explicit config**, no silent fallback between local
   and cloud (ADE parity). Profile config gains
   `transcription: {engine, whisper_model_id, deepgram_model, language}`.

## T1 — `haider-stt` + daemon key vault (engine wave)

1. Model-dir resolver with literal-path laws (3 platforms + both env
   overrides + the lowercase-Linux trap).
2. Model manager: catalog, streaming download with progress, sha256-before-
   rename, selected-model hint, whisper-cli discovery + install drivers.
3. Capture worker: cpal input thread, f32 mono, 3 s standby ring +500 ms
   preroll, ~60 ms envelope emitter (rms·0.78 + peak·0.22, clamp 0..1),
   16 kHz linear resample, WAV encoder. Digital-zero/stall watchdog with an
   honest "grant mic to your terminal" hint.
4. LocalWhisperEngine + DeepgramEngine per decisions 5–6, both emitting one
   provider-agnostic partial/final transcript stream.
5. Daemon: `FEATURE_TRANSCRIPTION_V1`, secret RPCs, vault storage, goldens.

## T2 — TUI talk UX

1. `/talk` + TalkChip live wiring (the chip exists; live press currently
   refused) — toggle-to-talk state machine: Esc cancels, Enter commits +
   submits, typing commits + continues. Reuse `model.listening` +
   `Composer::insert_str` seams.
2. **Right-to-left ASCII wave** (the centerpiece): fixed ring buffer sized to
   the wave width, newest sample at the RIGHT edge flowing left over time,
   8-level partial blocks `▁▂▃▄▅▆▇█`, perceptual sqrt mapping, asymmetric
   smoothing (attack 0.5 / decay 0.13) + noise-floor calibration on
   activation, gold slot when speaking / faint when quiet, composer-band
   placement, plain-glyph fallback style. Theme slots ONLY; rides the
   existing dirty-tick 30 fps loop — no new timers.
3. Partial-transcript ghost row above the composer (dim slot), replaced per
   partial, realized into the composer on commit.
4. Setup surface: engine picker, whisper model rows with download progress,
   Deepgram key paste → validate → fetch models → select, language field.
   Honest error states: mic denied (TCC hint), model missing, key invalid,
   endpoint error.
5. Laws: talk state machine reducer tests, wave ring/mapping/smoothing pins,
   engine mocks for partial/final streams, download integrity (bad sha
   refused), resolver literal paths, secret redaction. Executed-mutation
   notes per house rules.

## Ship

T1 → gate → T2 → gate → ladder → v0.0.x tag + install ritual + live probes:
real mic talk on this Mac (shared `ggml-base.en.bin` already on disk —
verify NO re-download happens), Deepgram path with a real key if the owner
provides one, wave renders right-to-left under both themes.
