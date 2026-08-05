# G2 — text-file attachments + session rename: implementation notes

Brief: `docs/briefs/G2-attach-rename-brief.md`. Seam map:
`docs/research/g2-attach-rename-seam-map.md`. Branch `g2-attach-rename`,
implemented at v0.0.71. All eleven locked decisions landed; deviations are
listed at the bottom.

## Part A — what shipped

- `AttachmentBlock::File { artifact, name, lines }` (protocol tool.rs) —
  additive tagged variant; existing variants untouched. `name` is the
  sanitized BASENAME only (≤ 120 chars, control chars stripped, no path
  separators), golden-pinned in `attachment_file.json`.
- haider-client `load_text_attachment` beside `load_image_attachment`
  (headless.rs): same 5 MiB bounded read, strict UTF-8 with the DISTINCT
  `unsupported_attachment_encoding` refusal (message names PDFs/binary as
  unsupported), returns bytes + line count + sanitized basename.
  `HeadlessAttachment` (Image | File) replaces the image-only request
  attachment vector; upload builds the matching wire block.
- TUI `/attach`: image sniff first (unchanged); ONLY on
  `unsupported_attachment_type` the text lane loads the file →
  `PendingKind::File { name, lines }` chip labeled `name · N lines` →
  `ready_block` mints `AttachmentBlock::File`. Registry description now
  says file, not image. Caps (5 chips, 5 MiB) unchanged. `haider run
  --attach` has the byte-identical fallback order (cli run.rs).
- Daemon `validate_turn_attachments`: one new File arm — name sanity
  BEFORE any CAS read, then CAS existence + per-object/aggregate caps
  (shared code), then a UTF-8 decode re-gate on the verified bytes (the
  client gate is never trusted). Refusals are `invalid_argument` with
  messages naming the exact failure.
- Worker `resolve_prompt_attachments`: File → CAS bytes → UTF-8 →
  REPLACED IN PLACE by `Block::Text` with
  `<file name="NAME" lines="N">\n…\n</file>`. The compaction lane
  (`prepare_compaction_messages`) calls the SAME `file_attachment_text`
  helper — parity by construction. All three adapters (anthropic wire,
  openai, gemini) grew the unresolved-File error arm; a File block can
  never reach a provider. `hooks.rs` maps File to `text/plain` metadata.

## Part B — what shipped

- Protocol: `SessionMetadataV1.title: Option<String>` (serde default +
  skip — legacy rows/receipts byte-identical) and
  `SessionConfigEventPayload::SessionRenamed { title }` (None = cleared,
  absent on the wire) with `session_renamed_value` /
  `session_renamed_from_value` helpers. Fact golden `session_renamed.json`.
- Wire: `RequestBody::SessionRename { command_id, session_id,
  worker_generation, title }` / `ResponseBody::SessionRename { session_id,
  title, renamed_seq, worker_generation }`,
  `FEATURE_SESSION_RENAME_V1 = "session_rename_v1"` (advertised in
  `welcome_features`), `SessionSummary.title` additive. The golden wire
  transcript grew exactly three appended frames (welcome + request +
  response); the D1/T1/U1 tail anchors were re-counted truthfully
  (6+7+3+3 / `len-13..len-6` / 3+3).
- Store: `rename_session` cloned from `select_session_model` — ONE
  transaction: receipt claim + `meta_json.title` UPDATE + `session_renamed`
  fact with `PromptRender::Omit`; identical worker-generation fence and
  receipt-replay preflight (`session_rename_receipt`). `only_if_untitled`
  (auto-title only) short-circuits to `Skipped` BEFORE any claim.
- Daemon: `session.rename` handler (control + control-attachment gates,
  `normalize_session_title`: strip control chars → trim → 80-char cap →
  empty ⇒ None), the actor `Rename` arm (publishes the committed fact and
  advances the in-memory head — the F3 CAS reads journal truth), and
  `session.list` surfacing the title both top-level and inside metadata.
- Auto-title: `AcceptedTurn.first_user_turn` (additive, serde-default
  false — pre-G2 receipts replay false) is computed in `accept_turn` as
  agent-less ∧ branch-less ∧ non-steer ∧ tree-parent-less. On such an
  accept (fresh AND receipt-replayed — crash-recovery safe), the handler
  issues the same rename command with the internal per-session id
  `auto-title-{session}` and a generation-/title-free digest, making it
  at-most-once forever; the slug mirrors the TUI's `slug_name` (first 3
  words, kebab, ≤ 28, fallback `session`). Best-effort: a failed
  auto-title never fails the committed turn. An explicit rename always
  wins; overwrite is impossible (rpc pre-check + store guard).
- F3 GUARD: `session_config_only_delta` needed NO code change — it decodes
  the whole `SessionConfigEventPayload` union, so `session_renamed` joined
  by construction. The membership is pinned by the new
  `worker_head_cas_tolerates_a_rename_fact_delta` law (and the executed
  mutation that narrowed the classifier to `model_selected` proved the
  law observes it).
- TUI: `/rename` stub replaced — gates (session-only, bare-arg usage
  flash, demo local fabricate, `session_rename_v1` stale-daemon notice)
  → `AppRequest::Rename` → durable `LiveCommand::Rename` (outboxed,
  session-scoped for reconnect resend) → reply applies the daemon's
  NORMALIZED title (optimism forbidden) + flash; typed refusals land on
  the exact session (`pending_rename`, F2e shape).
  `note_summary_counts` hydrates row names from `SessionSummary.title`
  ahead of its counts gate: launcher rows (`render.rs` `entry.name`),
  `/sessions`, and the attached header (`session_name`) all show wire
  titles; absence (older daemon) never clears.

## Laws (all runtime, all green)

- LA1 `file_attachment_is_inlined_with_header_and_never_reaches_the_provider`
  (daemond live_turn_rpc_tests) — inline + compaction parity + journal
  keeps the CAS ref.
- LA2 client `attach_text_loader_validates_utf8_and_sanitizes_the_name`
  (cli_tests) + daemon
  `file_attachment_utf8_and_name_sanity_enforced_at_acceptance`
  (session_hub_tests).
- LA3 oversize + name sanitization inside both LA2 tests (client cap,
  daemon path/empty/control/121-char refusals, head never advances).
- LA4 image regression: every existing image law untouched and green
  (vision refusal, compaction-no-image-bytes, mime allowlist, dangling
  ref, caps, b4b image chip flow).
- LB1 `rename_is_receipted_published_listed_and_replayed`.
- LB2 `stale_generation_rename_is_refused_and_mutates_nothing`.
- LB3 `auto_title_fires_once_on_first_accept_and_never_overwrites`.
- LB4 `worker_head_cas_tolerates_a_rename_fact_delta`.
- LB5 `session_list_title_hydrates_launcher_rows_and_sessions_listing`
  (+ the /rename wire law, refusal law, demo law in g2_rename_tests).
- Goldens: `attachment_file.json`, `session_renamed.json`,
  `session_rename_frames_are_additive_and_golden`, wire transcript +3
  frames (12 fixture lines, append-only, verified by diff).

## Deviations from the brief (and why)

1. `PendingKind::File` carries `{ name, lines }`, not `{ lines }` alone:
   `ready_block()` must mint `AttachmentBlock::File`'s `name` field and
   the chip label is not a parseable source of truth.
2. Daemon scenario 3 (`scenario_3_submit_streams_one_contiguous_durable_
   turn_over_real_uds`) now decodes the ONE interleaved additive config
   fact tolerantly (asserting it IS `session_renamed`) — the auto-title
   fact rides the live stream by design and the strict all-core-typed
   expectation predates the additive union.
3. Title over 80 chars is NORMALIZED (truncated) rather than refused —
   the brief's "Title ≤ 80 chars, trimmed, control chars stripped; empty
   normalizes to None" reads as one normalization pipeline; the response
   reports the committed normalized truth (LB1 pins this).
4. Auto-title on a legacy (pre-G2, untitled, multi-turn) session: never
   fires — `first_user_turn` is false for every non-first accept and
   legacy receipts replay it as false. This is the strict reading of
   "must NOT fire on subsequent turns"; such sessions stay unnamed until
   an explicit `/rename`.

## Ledger

`cargo run -p xtask -- test-count --update`: 1906 → 1920 (+14 — exactly
the fourteen tests named above).
