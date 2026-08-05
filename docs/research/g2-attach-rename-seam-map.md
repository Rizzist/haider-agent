# G2 seam map — file attachments + session rename (Explore agent, 2026-08-05, @ v0.0.71)

## A. Attachment pipeline today (images end-to-end, PastedText lane reusable)

- /attach registered: tui commands.rs:47-51 (CommandSpec :6-13, COMMANDS
  :16-129). Dispatch attach_command app.rs:3177-3213 — demo refusal,
  FEATURE_ARTIFACT_PUT_V1 gate (:3195; const rpc frame.rs:223), cap
  MAX_TURN_ATTACHMENTS=5 (app.rs:2448) → AppRequest::AttachRead (:1962).
- ONE filesystem touch: runtime.rs:2368-2394 attach_read_effects →
  haider_client::load_image_attachment →
  begin_attachment_upload(bytes, PendingKind::Image{mime}, label)
  (app.rs:3108-3130) → AppRequest::AttachUpload (:1970-1974, ArtifactBytes
  debug-redacted :2423-2442).
- Chips: composer.rs:40-90 PendingKind::{Image{mime}, PastedText{lines}},
  PendingAttachment; ready_block() :76-90 → protocol AttachmentBlock. Ops
  :308-385. Backspace removes newest chip (app.rs:4225-4230); submit
  refuses while uploading (:4383-4392).
- THE TYPE GATE: haider-client headless.rs:67-113 load_image_attachment —
  bounded read (5 MiB cap :56), sniff_image_mime :101-113 MAGIC BYTES ONLY
  (jpeg/png/gif/webp); error "unsupported_attachment_type" :88-94. Shared
  by TUI and `haider run --attach` (cli run.rs:130-138, 231-232).
- Daemon re-gate: session_hub/rpc.rs:4029-4116 validate_turn_attachments;
  IMAGE_ATTACHMENT_MIME_ALLOWLIST (mod.rs:143-144); caps rpc.rs:26-28
  (5/turn, 5 MiB each, 16 MiB aggregate); CAS existence :4075. PastedText
  passes with NO mime gate, size only (:4066); Skill reserved (:4067-4073).
- Wire: upload = LiveCommand::ArtifactPut (live.rs:114, :2746-2750) →
  RequestBody::ArtifactPut { data_base64 } (frame.rs:902-907); cap
  ARTIFACT_PUT_MAX_BYTES=8MiB (frame.rs:36); handler rpc.rs:435-501; reply
  → chip complete (live.rs:1393-1402 → app.rs:3132-3146). CAS = FileCas
  (event_store.rs:620, 3244).
- Canonical shape: AttachmentBlock (protocol tool.rs:93-115, tagged enum
  Image | PastedText { artifact, lines } | Skill) — refs only, bytes never
  ride submit. Submit: take_ready_attachments (app.rs:4537, 4593) →
  TurnSubmitWithBranch { attachments } (frame.rs:1014-1026). Errors
  frame.rs:155-166, ErrorData attachment variants :1631-1644.
- Journal: turn_submit rpc.rs:2795-2900 → TurnAcceptCommand →
  EventPayload::UserMessage { text, attachments, mode } (lib.rs:60-66);
  NodeKind::UserTurn { text, attachments } (history.rs:27-31).
- Prompt: prompt_history.rs:744-756 UserMessage → Block::Text +
  Block::Attachment. Worker resolve_prompt_attachments worker.rs:3419-3472:
  Image → CAS → ResolvedAttachment; PastedText → CAS → UTF-8 → REPLACED IN
  PLACE by Block::Text (:3443-3456) — never reaches adapters. Vision guard
  :3199-3218. Compaction parity :3474-3515.
- Adapters (all error on unresolved PastedText/Skill): anthropic
  wire/mod.rs:181-205 (+ MIME re-check :189-196); openai :1852-1861;
  gemini :453-465.
- Paste pill: ingress app.rs:3753-3762 (>3 lines or >300 UTF-16 units);
  big_paste :3227-3258 (CRLF normalize, 5 MiB cap, 5 chips,
  PendingKind::PastedText, "[Pasted N lines]").
- PDFs: ZERO existing path (would be new AttachmentBlock variant +
  per-provider document blocks — separate feature, not G2).

## A. Smallest correct text-file attach

1. haider-client: new load_text_attachment (bounded read, UTF-8 validate,
   5 MiB cap). TUI attach_read_effects: image sniff fails →  text
   fallback.
2. NEW AttachmentBlock::File { artifact, name, lines } (additive,
   tag="kind") so the model sees the filename; daemon validation adds one
   arm (UTF-8 + size); worker inlines as Block::Text with
   `<file name=…>` header — providers untouched.
3. Goldens: wire_golden_tests.rs + protocol golden_tests.rs + tui
   b4b_attach_tests.rs. Caps all exist already.
4. Update /attach desc text (commands.rs:47-51) + `haider run --attach`
   parity (cli run.rs).

## B. Session naming/rename

- SessionMetadataV1 (protocol session.rs:35-59): NO title field. Stored
  sessions.meta_json (insert event_store.rs:1285; read 867/1642/3730;
  update 1676). SessionSummary (rpc frame.rs:757-788): no name.
- AnnotationKind::{AutoTitle, Blurb…} (history.rs:63-67, 101-108) exists,
  UNUSED in live code (demo DemoEvent::AutoTitle script.rs:48,
  runtime.rs:1766-1770).
- TUI: SessionState.name (session.rs:45, demo kebab slug) + .title (:47
  demo blurb); checkout app.rs:8747-8748/8794-8795. LIVE rows minted
  NAMELESS (upsert_live_session app.rs:8892-8908 from session.list
  live.rs:1279-1305). Launcher row label = name.unwrap_or("session")
  render.rs:709 (blurb :747-751); /sessions label :8618. slug_name
  app.rs:52-72 (first 3 words, kebab, ≤28 chars).
- /rename ALREADY REGISTERED (commands.rs:127, help :402) but stubbed:
  app.rs:7838-7848 ("fork"|"rename" => flash "the daemon wave (W3)").
- Clone template — session.select_model: RequestBody::SessionSelectModel
  frame.rs:1086-1094 / response :1411; daemon rpc.rs:2583-2684; store txn
  event_store.rs:1610-1725 (receipt claim + UPDATE meta_json + append
  SessionConfigEventPayload::ModelSelected with PromptRender::Omit,
  :1683-1704). SessionConfigEventPayload union session.rs:80-97 —
  SessionRenamed { title } slots in additively.

## B. Smallest correct implementation

1. Protocol: SessionMetadataV1.title: Option<String> (serde default) +
   SessionConfigEventPayload::SessionRenamed { title }.
2. Wire: RequestBody::SessionRename { command_id, session_id,
   worker_generation, title } + response + FEATURE_SESSION_RENAME_V1 +
   SessionSummary.title (additive).
3. Daemon: clone session_select_model → session_rename + store txn beside
   select-model; surface title in session.list builder. New config fact
   joins session_config_only_delta (actor.rs:687-723) so F3 compaction
   tolerance holds.
4. TUI: replace stub with rename_command → durable-command path; hydrate
   entry.name from SessionSummary.title in note_summary_counts /
   upsert_live_session; launcher + /sessions display follow free.
5. Auto-title: daemon-side — on turn accept, if meta.title is None, derive
   slug from first user message in the SAME transaction pattern and journal
   session_renamed. Explicit /rename overwrites. (Model-generated blurb via
   AnnotationKind stays future work.)
