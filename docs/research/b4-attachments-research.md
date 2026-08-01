# B4 research — attachments live

Fable seam research, 2026-08-01. Line numbers approximate (b2-branches
lane was mutating the tree during the scan).

## Q1 — protocol shapes: fully defined, ref-based

- `AttachmentBlock` (haider-protocol/src/tool.rs ~93-115), tagged enum:
  `Image { artifact: ArtifactRef, mime, width?, height? }`,
  `PastedText { artifact, lines }`, `Skill { name, version_hash }`
  (reserved). Design law in the doc comment: "The tree stores refs,
  never copies; the prompt compiler injects content."
- `ArtifactRef` (ids.rs ~70-73): `blake3:<64 hex>` CAS address. No
  bytes/path/base64 on protocol shapes.
- Journaled twice per turn: `EventPayload::UserMessage { attachments }`
  (lib.rs ~58-64) and `NodeKind::UserTurn { attachments }` (history.rs
  ~26-31). Compiler renders Block::Attachment in the user message
  (prompt_history.rs ~748-756). Provider resolved shape:
  `ResolvedAttachment { artifact, data_base64 }` on TurnRequest
  (haider-provider/src/lib.rs ~147-168). `CapabilityDoc.vision` exists
  (provider.rs ~131-140). This is the A2 attachment schema (cited in
  docs/briefs/W2b-C1-anthropic.md).

## Q2 — daemon path: complete RPC→provider; NO byte ingress

- `RequestBody::TurnSubmit.attachments` (frame.rs ~777-788) →
  turn_submit (rpc.rs ~1611-1693; attachments are part of the durable
  command identity digest) → TurnAcceptCommand (event_store.rs
  ~215-231) → acceptance journals refs atomically (~1784-1855, steer +
  queued) → start_turn compile_with_artifacts →
  `resolve_prompt_attachments` (worker.rs ~3278-3331): Image → CAS
  get_artifact → base64 (dedup by artifact); PastedText → CAS → UTF-8
  → rewritten IN PLACE to Block::Text (never hits adapters); Skill →
  InvalidArgument. `config.attachments` set ~3128; ALSO cloned into
  DaemonContextCompactor.attachments (~3138, and manual path ~2628) —
  the compactor RE-SENDS attachments in its summarization request.
- CAS: FileCas at <profile>/cas/<2-hex-shard>/<hex> (cas.rs), hash-
  verified reads; HubStoreHandle::{put_artifact, put_artifact_file,
  get_artifact} + ArtifactReader (session_hub/mod.rs ~2237-2256,
  ~2149-2157).
- Validation: essentially NONE — no mime allowlist, no size caps, no
  artifact-existence check at acceptance (dangling ref fails the run
  at start_turn).
- THE GAP: no RPC uploads bytes into the daemon CAS (full method list
  frame.rs ~706-996 — nothing like artifact.put). Clients can name a
  ref but cannot make the bytes exist.
- Vocabulary trap: session_hub "attachments"/max_attachments_per_
  connection = event-stream subscriptions, unrelated.

## Q3 — provider encoding: implemented + tested, all three wires

- Anthropic (wire/mod.rs ~119-152): user-role Image → base64 source
  block; mime allowlist jpeg/png/gif/webp; non-user-role images /
  unresolved PastedText / Skill → typed invalid_request. Attachment
  index ~67-83.
- OpenAI Responses (openai.rs ~1669-1693): input_image data URL,
  detail auto. No input_file/PDF anywhere. No mime allowlist.
- Chat completions (~1868-1877): image_url data URL part.
- Vision capability declared (Anthropic + first-party OpenAI Native;
  openai-compatible + Fake Unsupported) but NEVER consulted by any
  caller. Catalog has zero vision knowledge.
- Tests: anthropic_provider_tests ~171-246 encoding; live image_in
  gate; openai tests cover only empty attachments.

## Q4 — TUI: nothing live; count-only vocabulary

- Both submit paths hardcode `attachments: vec![]` (app.rs ~3281,
  link.rs ~543-556). AppRequest::SubmitText has no attachment field.
- Paste (app.rs ~2572-2613): bracketed only; >3 lines or >300 UTF-16
  units → literal "[Pasted N lines] " pill and the content is
  ZEROIZED AND DROPPED (sim-era theater; tool.rs's intended
  PastedText-CAS vocabulary was never wired). No image paste, no
  drag-drop, no /attach.
- Render vocabulary EXISTS: " [+N attachment(s)]" user rows
  (projection.rs ~42/~369, render.rs ~4192, plain.rs ~32).

## Q5 — headless: no flag

- run.rs flags ~74-135: no attach. headless.rs ~676-684 submits
  attachments: Vec::new(); submit body is immutable across retries so
  attachments must be in the durable command identity from first send.
- haider-cli already deps haider-store (could write FileCas directly)
  but that bypasses daemon single-writer discipline — use the RPC.

## Q6 — limits + footprint

- Estimator serializes (messages, system_prompt, tools, attachments)
  at bytes/4 (actor.rs ~3852-3874): a 5 MB image ≈ 1.7M "tokens" —
  blows the W7 threshold, can wedge compaction; compactor re-inflates
  by re-sending attachments. No size caps anywhere in the chain.

## Q7 — prior art

No B4 docs in-tree. A2 schema via W2b-C1. Paste-pill intended
vocabulary in tool.rs comment.

## Recommended seam plan

Dark path already works: submit refs → journal → compile → CAS resolve
→ encode (all 3 wires) → compact/estimate. Missing/broken, in order:
1. **artifact.put RPC** (content-addressed → naturally idempotent, no
   command receipt; must respect negotiated frame_limit — chunk or
   bound; wired to HubStoreHandle::put_artifact/FileCas).
2. **Acceptance validation** in turn_submit: artifact EXISTS in CAS,
   mime allowlist, per-attachment + per-turn byte/count caps —
   today a dangling ref or 50 MB image is durably accepted and only
   fails later.
3. **Image-aware token estimate**: images count as fixed vision-token
   estimates, not base64/4; compactor policy for attachments (exclude
   images from summarization requests to kill compounding cost).
4. **Vision gating**: consult capabilities().vision → graceful typed
   local refusal instead of provider-side 4xx.
5. **Headless** `--attach <path>` (repeatable): sniff mime, upload,
   blocks in the immutable submit body.
6. **TUI** (SEPARATE UI LANE, not codex): /attach + real paste pill
   (PastedText artifact) + pending blocks on the draft + existing
   [+N attachment(s)] rendering.

Biggest risk: W7 size interaction (base64-counted estimates +
compactor re-inflation). Secondary: vision gating absent.
