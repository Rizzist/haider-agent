# T1 — `haider-stt` engine crate + daemon key vault — implementation notes

Implementer: Fable 5. Branch `t1-stt-engine`, built on `main` @ `652e008`
(v0.0.68). Scope contract: `docs/briefs/T-wave-talk-transcription-brief.md`
(T1 + locked decisions); mechanics authority:
`docs/research/t-wave-transcription-research.md`. T2 (talk UX, wave, setup
card) is deliberately NOT here — no TUI file changed.

## Crate layout (`crates/haider-stt`)

- `lib.rs` — charter, `EngineKind` (`whisper-local`/`deepgram` labels),
  provider-agnostic `TranscriptFrame`, `TranscriptionResult` (ADE
  `WhisperTranscriptionResult` shape), typed `SttError` (ModelMissing /
  RuntimeMissing / MicUnavailable / ChecksumMismatch / Unauthorized / … —
  honest first-class states, no variant ever carries secret bytes).
- `model_dir.rs` — the byte-for-byte ADE resolver (locked decision 3):
  `RUST_DIFFFORGE_DATA_DIR` (verbatim, NO suffix) → `RUST_DIFFFORGE_HOME`
  (verbatim) → macOS `~/Library/Application Support/DiffForge` / Windows
  `%APPDATA%\DiffForge`→`%LOCALAPPDATA%\DiffForge` / Linux
  `$XDG_DATA_HOME/diffforge`→`~/.local/share/diffforge` (LOWERCASE), then
  `/whisper`. Platform-parametric core over an injected env snapshot so the
  literal-path laws cover all three platforms from one host; empty env
  values are unset (ADE `cloud_mcp_env_path`); home = `HOME` → `USERPROFILE`.
- `catalog.rs` — the exact ADE three-model table (tiny.en/base.en/small.en:
  ids, `ggml-<id>.bin` files, HF URLs, sha256, disk/mem MB, tiers).
  `selected_model_hint` reads `selected-model.txt` as a HINT (trimmed,
  case-insensitive, unknown → None) and NOTHING in the crate writes it.
  `effective_model`: own selection → ADE hint → `base.en`. Installed-state
  answers are per-call filesystem truth (the ADE's uninstall evicts the
  whole dir at any moment).
- `download.rs` — `install(client, dir, spec, progress)`: existing final
  file short-circuits with ZERO network I/O (shared-install law); stream →
  `<file>.download` → sha256 over the closed temp → atomic rename; mismatch
  removes the temp and the final path never exists; progress
  starting/downloading(monotonic bytes)/done; 900 s budget.
- `runtime.rs` — discovery ladder `<whisper>/runtime/` (recursive) → PATH →
  well-known list; names `whisper-cli`→`main`→`whisper` (`.exe` on
  Windows); ADE literal well-known paths per OS. Install drivers: macOS
  `brew install whisper-cpp` (missing brew → honest hint outcome; non-zero
  exit → first output line only), Windows pinned v1.8.4 zip+sha256 through
  the same verify-before-rename install, entry names screened by an
  explicit zip-slip guard BEFORE extraction (platform `tar` does the
  unpack; bsdtar reads zip), Linux → PATH-only hint outcome.
- `capture.rs` — deterministic `CaptureState` (every transition takes an
  explicit `Instant`, so laws run without a microphone): ADE DSP ports
  (mono mix, rms/peak stats, dBFS, digital-zero detect, single-value
  envelope = mean-removed `rms·0.78 + peak·0.22` clamp 0..1 over the 768-
  sample window, linear resample with ADE rounding, byte-identical WAV
  encoder, asymmetric linear16). 3 s standby ring; record-start seeds
  exactly the last 500 ms; envelope cadence 60 ms recording / 1 s standby;
  900 s capture cap reported once; digital-zero (1.5 s) and stall (2 s)
  watchdogs with the honest terminal-mic-grant hint, one report per
  episode + Recovered. PRIVACY LAW: `Frames` events exist only while
  recording. `CaptureWorker` is the thin cpal glue (default input device,
  F32/I16/U16, dedicated thread, command channel) — the only
  cpal-touching seam, untestable in CI and kept logic-free on purpose.
- `chunker.rs` — faithful port of `native_partial_ingest_samples`:
  adaptive noise floor (dB, blend 0.01/0.05), speech `rms_db > floor+10dB
  (≥-45)` or `peak ≥ 0.035`, quiet `≤ floor+6dB (≥-50)` and `peak <
  0.025`; cut on ≥750 ms quiet past 10 s, force at 35 s, 1.2 s min tail,
  speechless chunks NEVER emitted, forced final-tail flush.
- `policy.rs` — ADE hallucination policy defaults (900 ms / rms 0.01 /
  peak 0.02, bracketed ≤24, no-speech markers, low-energy "you" ≤4 chars /
  1 word — gated on LOW-ENERGY captures only), stderr warm-up prefix
  filter + whitespace-collapsing normalize, 12-word overlap-deduplicating
  `join_partial_text`.
- `local.rs` — `LocalWhisperEngine`: exact ADE argv
  (`-m -f -l -t -nt -np -bo 1 -bs 1 -nf [--prompt]`), threads 4..=8
  (partial ≤4), warm page-cache once per (path,size) with NO retained
  handle, per-chunk fresh spawn (tokio process, kill_on_drop, 180 s
  budget, cancel token kills mid-flight), 32 MiB input cap before spawn,
  stdout transcript / filtered-stderr errors, model existence re-verified
  at EVERY spawn (eviction → typed `ModelMissing`). Partial session:
  chunker → 16 kHz resample → WAV → policy → spawn; cumulative assembled
  frames (`is_final:false`), one final cumulative frame when text exists;
  ADE error semantics (errors surface only when nothing was assembled);
  900 s session audio cap.
- `deepgram.rs` — `clean_api_key`/`clean_language` (ADE hygiene, no key
  echo), pinned `realtime_url` (selected model + language + linear16 +
  native rate + channels=1 + interim_results + smart_format,
  percent-encoded), `validate_key` = `GET /v1/auth/token` (401/403 →
  typed Unauthorized), `fetch_streaming_models` = `GET /v1/models`
  filtered `streaming: true` minus Flux (architecture or name), WS session
  (Token header, 10 s connect gate, binary i16 LE frames, KeepAlive every
  4 s, CloseStream + ≤8 s drain, finals-join result with interim
  fallback, 900 s self-finalizing cost cap). Origins/budgets injectable
  for loopback fixtures; production constants pinned.
- `config.rs` — the profile `transcription` section (locked decision 7):
  `{engine: local|deepgram, whisper_model_id?, deepgram_model?, language}`
  inside `<store_dir>/config.json`. Load: absent → defaults; a PRESENT
  corrupt section is a TYPED error (silent defaults would flip the user's
  explicit engine choice). Save: read-modify-write over the raw JSON value
  (foreign keys preserved), temp + atomic rename. The Deepgram key itself
  never touches this file.

## Daemon key vault (locked decision 4)

- `haider-rpc`: `FEATURE_TRANSCRIPTION_V1` (`transcription_v1`),
  `RequestBody::TranscriptionSecretGet` (unit),
  `RequestBody::TranscriptionSecretSet { secret: SecretWire, clear }`,
  `ResponseBody::TranscriptionSecretGet { secret: Option<SecretWire> }`
  (absent stays OFF the wire), `ResponseBody::TranscriptionSecretSet
  { present }`. SecretWire is CONSUMED, not redefined — redacted Debug +
  zeroize-on-drop + zeroizing codecs come for free.
- `haider-daemon`: both methods gate through the existing
  `secret_surface_facade` (same-UID local UDS + vault-supported), then
  Control. Storage is the profile-scoped vault (`AccountsFacade` gained
  `vault: Option<Arc<dyn Vault>>` — the ProfileVault wrap, so the physical
  key is `blake3(profile)[..16]::transcription.deepgram` in the FileVault).
  Inline like `vault.stage`: one bounded ≤512-byte file op, comparable to
  one store transaction. ADE key hygiene BEFORE any write (trim, non-empty,
  ≤512, no control bytes; `clear:true` requires an empty secret); refusals
  never echo key material; missing key answers an honest `secret: None`.
  Deliberately NON-durable command-wise: no receipt may carry a secret —
  the vault file is the durable truth.
- Goldens: 7 frames appended to the wire transcript (set → present:true,
  get → secret, get-empty → no key, clear → present:false); fixture
  regenerated APPEND-ONLY (28 inserted lines, 0 changed). New golden law
  pins the tail order, Debug redaction both directions, unknown-field
  tolerance, absent-`clear` default, and the no-secret-key set response.
  The D1 tail-count pin was extended (6 → 6+7) — its intent (nothing
  before the D1 block moved) is preserved and re-stated.
- `welcome_features()` + its pin test advertise `transcription_v1`.

## Laws added (ledger 1704 → 1783, +79)

Every seam carries non-vacuous laws with MUTATION CHECK docs: resolver
literal paths (3 platforms × both overrides × lowercase trap × empty-env ×
no-home), catalog byte-pins + read-only-HINT (non-normalized sidecar bytes)
+ precedence, download progress/verify-before-rename/short-circuit-zero-
network/typed-500, discovery ladder + name order + well-known literals +
brew exit/first-line + zip-slip screen + archive-without-binary, capture
DSP pins (envelope blend, DC-removal, WAV golden bytes, asymmetric i16,
resample rounding) + preroll + cadence + frames-privacy + cap-once +
watchdog episodes, chunker cadence laws (no-cut-before-10s, 750 ms cut at
the exact batch, 35 s force, speechless-never, contiguous indices), policy
drop table + healthy-"you"-kept + join dedup, local argv pin + stub-CLI
roundtrip + filtered stderr + per-spawn eviction + oversize + cancel +
cumulative session + error-only-when-empty, deepgram URL/auth pins + model
filter + validation statuses + message semantics + full WS session contract
+ pre-finish cost-cap, config defaults/typed-corruption/foreign-key-
preserving save, daemon FileVault roundtrip (physical alias) + UDS-and-
Control-only + hygiene-before-write + welcome pin + golden tail/redaction/
tolerance.

## Verification

- `cargo fmt --all` clean; `cargo clippy --workspace --all-targets` zero
  warnings; full `cargo test --workspace` green (ulimit 8192,
  CARGO_INCREMENTAL=0); `xtask check` green (baseline 1783).
- Mutation campaign: 13 EXECUTED kills, 2 of them after closing vacuous
  laws found by survivors — see `T1-stt-engine-mutation-notes.md`.

## Honest deviations / notes

1. Scratch WAVs go to `$TMPDIR/haider-stt` (tempfile, auto-removed), NOT
   the ADE's shared `whisper/recordings/` — the shared contract covers
   models/runtime/selection only, and writing into the ADE's transient dir
   would race its cleanup.
2. The chunker's `min tail` guard is ported for parity but unreachable
   through the public flow (quiet-gap/max-length cuts always exceed it and
   the final flush is forced, exactly as in the ADE) — documented, not
   counted as a law.
3. `capture::CaptureWorker` (the cpal glue) has no automated law: it needs
   a real input device. All behavior it delegates to `CaptureState` is
   law-covered; the glue is kept logic-free. Live-mic verification is the
   ship-gate probe after T2.
4. Windows zip extraction shells the platform `tar` (bsdtar reads zip;
   ships on Win10+); entry names are screened for zip-slip before any byte
   is extracted. No zip crate was added (deps stayed cpal +
   tokio-tungstenite + futures-util).
5. Deepgram sessions emit per-message frames only (ADE parity); the
   assembled text is `finish()`'s result. Local sessions add one final
   cumulative frame when text exists (also ADE parity).

## Unfinished (for later lanes)

- T2 entirely: `/talk`, TalkChip wiring, the right-to-left wave, ghost
  row, setup surface, engine selection UX — nothing in `haider-tui`
  consumes `haider-stt` yet (the crate compiles into the workspace but has
  no consumer edge until T2).
- No client-side (`haider-client`) helper for the two new RPCs; T2 will
  drive them through its existing Link plumbing.
- Live probes (real mic, real Deepgram key, real whisper-cli + shared
  `ggml-base.en.bin` no-redownload check) belong to the T-wave ship gate,
  not this lane.
