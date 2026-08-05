# G2 — file attachments beyond images + session naming/renaming

Owner contract, verbatim: "support attaching files, renaming/naming
sessions". Authority: `docs/research/g2-attach-rename-seam-map.md`
(file:line seams). Branch: `g2-attach-rename`. Read the seam map BEFORE
writing code.

## Part A — text-file attachments

1. haider-client: `load_text_attachment` beside `load_image_attachment`
   (headless.rs): bounded read (same 5 MiB cap), strict UTF-8 validation,
   returns bytes + line count + basename. Reject non-UTF-8 with a
   distinct error (`unsupported_attachment_encoding`). PDFs remain
   unsupported — out of scope, error message may say so.
2. NEW `AttachmentBlock::File { artifact, name, lines }` (protocol
   tool.rs — additive, tagged enum; do NOT touch existing variants).
   `name` is the basename only (never a full path — privacy), ≤ 120
   chars, sanitized (no control chars).
3. TUI `/attach <path>`: try image sniff first (existing); on
   `unsupported_attachment_type`, fall back to text load →
   `begin_attachment_upload` with a new `PendingKind::File { lines }`
   chip labeled `name · N lines`. Update the /attach command description
   (commands.rs:47-51) to say file, not image. Cap logic (5 chips, size)
   unchanged. `haider run --attach` gets the same fallback (cli run.rs).
4. Daemon validate_turn_attachments: one new arm — File requires CAS
   existence + size cap + UTF-8 (decode check) + name sanity. Aggregate
   caps unchanged.
5. Worker resolve_prompt_attachments: File → CAS bytes → UTF-8 →
   replaced in place by `Block::Text` with header
   `<file name="NAME" lines="N">\n…\n</file>` (PastedText pattern,
   worker.rs:3443-3456). Providers must NEVER see a File block —
   extend the three adapter error arms to include it. Compaction path
   (worker.rs:3474-3515) gets the same inlining.

## Part B — session naming/renaming

6. Protocol: `SessionMetadataV1.title: Option<String>` (serde default,
   skip-serializing-if None — legacy rows unaffected) +
   `SessionConfigEventPayload::SessionRenamed { title: Option<String> }`
   (None = cleared). Title ≤ 80 chars, trimmed, control chars stripped;
   empty string normalizes to None.
7. Wire: `RequestBody::SessionRename { command_id, session_id,
   worker_generation, title }` + response + `FEATURE_SESSION_RENAME_V1`
   const + `SessionSummary.title: Option<String>` (additive) so
   session.list carries it.
8. Daemon: clone `session_select_model` (rpc.rs:2583-2684) →
   `session_rename` with the same receipt-replay idempotency; store txn
   beside select-model (event_store.rs:1610-1725): receipt claim +
   UPDATE meta_json.title + append SessionRenamed with
   PromptRender::Omit. Generation fence identical to select_model.
9. F3 GUARD (critical): SessionRenamed must join the
   `session_config_only_delta` classifier (session_hub/actor.rs:687-723)
   so a rename mid-compaction cannot wedge the head CAS. Extend the
   existing worker_head_cas tolerance law with a rename-delta case.
10. Auto-title: daemon-side, on first turn ACCEPT for a session whose
    meta.title is None, derive `slug of first user message` (first 3
    words, kebab, ≤ 28 chars — mirror tui app.rs:52-72 slug_name) and
    journal the same SessionRenamed fact (internal command id, receipt
    pattern). An explicit /rename later overwrites. Auto-title must NOT
    fire on subsequent turns nor overwrite an existing title.
11. TUI: replace the /rename stub (app.rs:7838-7848) with a real
    `rename_command(remainder)` → durable-command path → on reply set
    `self.session_name` + flash. Hydrate `entry.name` from
    `SessionSummary.title` in note_summary_counts / upsert_live_session
    (app.rs:8892-8945) so launcher rows (render.rs:709) and /sessions
    (app.rs:8618) show real titles. `/rename` with no arg → usage flash.
    Bare `/rename` clearing is NOT supported (explicit `<name>` only).

## Mandatory laws (runtime)

- LA1 end-to-end text attach (daemon runtime): submit turn with a File
  attachment → UserMessage journals it → resolve_prompt_attachments
  inlines Block::Text with the header → provider request contains the
  file text, zero attachment blocks.
- LA2 non-UTF-8 rejected at client AND daemon arms (two tests).
- LA3 oversize + name-sanitization rejections.
- LA4 image path REGRESSION: existing image attach laws still green
  untouched.
- LB1 rename RPC: meta_json.title updated + fact journaled + session.list
  carries title; receipt replay idempotent (duplicate command_id → same
  outcome, no double fact).
- LB2 stale worker_generation refused, mutates nothing.
- LB3 auto-title on first accept; second turn does NOT re-title;
  explicit rename wins over auto-title; auto-title never overwrites.
- LB4 rename-delta tolerated by the compaction head CAS (extend F3 law).
- LB5 TUI: launcher row + /sessions render the wire title (projection or
  app-level test in the existing style).
- Goldens: protocol fixtures for the new AttachmentBlock::File and
  SessionRenamed; rpc wire transcript WILL grow the SessionRename
  request/response — regenerate honestly and re-anchor tail assertions
  per wire_golden_tests.rs conventions (never fudge counts).

## Discipline (non-negotiable)

Same as every lane: CARGO_INCREMENTAL=0; per-crate tests for touched
crates; `cargo fmt --all -- --check` clean at every commit; ledger
`cargo run -p xtask -- test-count --update` before final commit with a
truthful old → new in the message; write
`docs/briefs/G2-attach-rename-notes.md` +
`docs/briefs/G2-attach-rename-mutation-notes.md` (executed mutations
only: commit first, single-anchor mutation, run the ONE named test with
"running 1 test" observed, record failure, revert, re-run green; ≥ 5
executions across: File inlining, daemon File validation, rename store
txn, auto-title guard, config-only-delta membership). Do NOT: bump
versions, tag, touch MCP, rename existing variants, delete
~/.codex/sessions.
