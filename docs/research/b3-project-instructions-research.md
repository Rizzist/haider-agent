# B3 research — project instructions (AGENTS.md / HAIDER.md)

Fable seam research, 2026-08-01. Line numbers approximate (tree was
shifting under the b2-branches lane during the scan).

## Q1 — system prompt assembly: ONE canonical point

- `SystemPromptBuilder` in crates/haider-daemon/src/worker.rs (~512-527):
  `build(metadata: &SessionMetadataV1) -> String` = VERSION line
  (`haider-system-v1`, recorded in session metadata at creation as
  `system_prompt_version`), identity line, `Workspace: {metadata.cwd}`,
  tool/effect policy sentence. Contract (worker.rs ~505-511): same
  metadata → same prompt; every provider request in one pinned logical
  turn receives the same non-None prompt; policy changes are versioned
  facts, never silent drift. Provider adapters must not invent policy.
- Injection: `start_turn` sets `config.system_prompt =
  Some(SystemPromptBuilder::build(metadata))` (worker.rs ~2940); the
  compaction path rebuilds it into
  `DaemonContextCompactor.post_compaction_system_prompt` (worker.rs
  ~2548; struct field ~128) for the post-compaction fit check.
- Carrier: `HarnessConfig.system_prompt: Option<String>`
  (haider-core/src/actor.rs ~105, default None ~161), cloned into every
  `TurnRequest` (~1059-1066). Provider boundary is pure wire encoding:
  OpenAI Responses → `instructions` (openai.rs ~1751-1756); chat
  completions → system message (~1802-1807); Anthropic → top-level
  `system` (wire/mod.rs ~58-60). Tools ride separately
  (`config.tools = tool_factory.definitions()`); no date/account
  identity in the prompt today. haider-cli main.rs standalone embedding
  and accounts.rs login probes leave system_prompt None.

## Q2 — working directory

- Durable: `SessionMetadataV1.cwd` — canonical absolute UTF-8 workspace
  path (haider-protocol/src/session.rs ~35-37), persisted in
  sessions.meta_json by the atomic create transaction
  (event_store.rs ~1174-1195). Validated at the wire by
  `validate_workspace` (session_hub/rpc.rs ~2489-2514): absolute,
  fs::canonicalize'd, UTF-8, a directory; descriptor held open across
  the create transaction.
- Origin: TUI uses current_dir() (haider-cli/src/main.rs ~334, 436);
  headless `HeadlessRunRequest.cwd` filled in run.rs ~201-211.
- Tools confine UNDER the root: EffectBroker rooted at metadata.cwd
  (worker.rs ~3529; broker.rs ~761-784); process_exec rejects cwd not
  starts_with(workspace_root) (process.rs ~95-113). Children inherit
  parent cwd verbatim (delegation.rs ~157-181).
- CAVEAT: every existing file-access path is confined under the
  workspace root. A CLAUDE.md-style UPWARD parent walk is a new
  capability outside the broker's confinement model → must be
  daemon-owned policy, never a broker effect.

## Q3 — file-read infra

- fs_read exists (filesystem.rs ~94-122) with
  resolve_workspace_path/require_under_root (~1717-1781: canonicalize,
  symlink-refusing missing-leaf, boundary error) and UTF-8-enforced
  openat reads (~582-604) — but NO byte cap on the read itself; result
  bounding is downstream (ResultBounds max_preview_bytes 8 KiB + CAS
  overflow).
- Bounded-read precedents to imitate: oauth.rs bounded_response
  (~2288); catalog.rs MAX_CATALOG_BYTES = 1 MiB (~33-35); file_vault.rs
  MAX_SECRET_BYTES = 512 KiB; accounts/oauth.rs MAX_BUNDLE_BYTES
  256 KiB / MAX_FIELD_BYTES 64 KiB.
- No ready-made capped doc reader; B3 loader combines filesystem.rs
  canonicalize/UTF-8 discipline + hard cap. fs_read is a journaled
  broker effect — the B3 loader is daemon-initiated and reads directly
  (never synthesizes a model tool call).

## Q4 — durability model

- Journal: EventEnvelope per-session seq; frozen forward-compat
  (unknown payload kinds tolerated via RawEnvelope; writers never
  remove/re-type). Freshest additive exemplar: BranchCreated
  (protocol lib.rs ~75-78, branch.rs ~24-29) — emitted atomically with
  registry row + command receipt.
- session.create = atomic receipt + meta_json + Created@seq1 with
  PromptRender::Omit (event_store.rs ~1129-1245). SessionMetadataV1 is
  additive-friendly (system_prompt_version, permission_overrides both
  serde-default/skip).
- The compiled projection (PromptHistoryCompiler) renders ONLY
  committed tree events; the system prompt rides HarnessConfig OUTSIDE
  the tree, recomputed per turn from immutable metadata.
- Two conforming homes for B3 text: (a) additive SessionMetadataV1
  field captured at create (bit-stable replay, stale files); (b)
  additive EventPayload fact per turn (BranchCreated-style, committed
  at/next to turn acceptance) so recovery reproduces the exact prompt.
  Composed text keeps riding HarnessConfig.system_prompt either way.

## Q5 — refresh precedent

- Re-resolved once per LOGICAL TURN, never mid-turn: provider/account
  (R6 pinning, worker.rs ~2830-2833; "manual login changes affect the
  next logical turn" ~105-112), model context_window, tool inventory.
- Recomputed per turn deterministically: the system prompt itself.
- Captured once at create, immutable: cwd/provider/model/max_tokens/
  permission_overrides/system_prompt_version.
- Strongest precedent for B3: RE-READ AT TURN START, pinned for the
  whole logical turn.

## Q6 — prior art

Zero hits for AGENTS.md / HAIDER.md / CLAUDE.md / "project
instructions" anywhere. Nearest: catalog.rs cap comment (codex
catalogs embed per-model base instructions — motivates the 1 MiB cap
only); SystemPromptBuilder description in docs/research/w3c report.

## Q7 — W7 size interaction

- system_prompt is ALREADY counted: estimate_provider_request_input_
  tokens serializes (messages, system_prompt, tools, attachments) at
  bytes/4 (actor.rs ~3852-3874) → estimated footprint; provider-
  reported Exact covers it too. W7b threshold min(85%, window−reserve)
  (~3771-3777). Manual-compaction fit check includes
  post_compaction_system_prompt + tools (worker.rs ~287-307).
- Consequence: instructions raise the INCOMPRESSIBLE floor (compaction
  never summarizes the system prompt) → hard caps required.

## Recommended seam plan (the B3a shape)

1. Daemon-side loader module beside worker.rs: from canonical
   metadata.cwd walk UPWARD collecting HAIDER.md / AGENTS.md
   (cwd-first, then parents to filesystem root with a depth stop;
   nearest-file-last so deeper = higher precedence in the composed
   block), each read canonicalized, UTF-8, symlink-cautious, hard
   per-file + total byte caps (~32-64 KiB, precedent MAX_FIELD_BYTES).
   Daemon policy — NOT a broker effect, no effect journal entry.
2. Compose in exactly one place: SystemPromptBuilder gains the loaded
   block; VERSION bumps to haider-system-v2. Adapters unchanged.
3. Refresh: load at start_turn beside resolve_for_turn/definitions()
   — file edits take effect next logical turn; one pinned turn sees
   one prompt. Journal an additive fact of what was loaded
   (paths + digest + byte counts), PromptRender::Omit, so replay can
   prove which instructions shaped a turn without re-reading a
   mutated filesystem.
4. Feed the same composed prompt into post_compaction_system_prompt so
   the W7 fit check stays honest. Token accounting needs no change.

DECISION (made by the run, per precedent): re-read-per-turn-boundary
with a journaled additive fact — matches R6 pinning + tool inventory,
gives live-edit UX, keeps replay provable.
