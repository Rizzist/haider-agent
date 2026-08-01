# B3 — project instructions: HAIDER.md / AGENTS.md into the system prompt

AUTHORITY: docs/research/b3-project-instructions-research.md (read
WHOLE, first). Its seam plan binds; line numbers are approximate —
re-locate every seam before editing (B2a may have shifted worker.rs).

## Scope (haider-daemon + protocol/store as needed — NO haider-tui)

1. **Loader module** (new, beside worker.rs): from the session's
   canonical `metadata.cwd`, walk UPWARD to the filesystem root
   (bounded depth stop) collecting `HAIDER.md` then `AGENTS.md` per
   directory (HAIDER.md wins within a directory; nearest-to-cwd files
   compose LAST so deeper instructions take precedence). Reads use the
   filesystem.rs discipline (canonicalize, UTF-8-enforced, symlink-
   cautious) with hard caps: 48 KiB per file, 96 KiB total (truncate at
   a UTF-8 boundary with an explicit `[truncated]` marker; a file over
   cap still contributes its capped prefix). Daemon-owned policy read —
   NOT a broker effect, no effect-journal entry, never a model tool
   call. Missing/unreadable files are silently skipped (a NOTICE line
   to daemon log only); an empty walk yields None and the v1 prompt
   composition is byte-identical to today.
2. **One composition point**: `SystemPromptBuilder::build` gains the
   instruction block (clearly delimited: a `Project instructions
   ({path}):` header per file, in composition order). VERSION bumps to
   `haider-system-v2`. Provider adapters MUST NOT change — they are
   wire encoding only.
3. **Refresh law**: load at `start_turn` beside R6 provider resolution
   and `definitions()` — once per logical turn, pinned for the whole
   turn (retries/rounds see identical bytes). Edits to the files take
   effect on the NEXT logical turn.
4. **Durable fact**: journal an additive `EventPayload` fact per
   logical turn recording what was loaded — ordered (path, blake3
   digest, bytes, truncated flag) entries — committed with
   `PromptRender::Omit`, so replay/recovery can prove which
   instructions shaped a turn without re-reading a mutated filesystem.
   Emit it ONLY when the loaded set is non-empty OR differs from the
   previous turn's fact (no per-turn noise for the common unchanged
   case — an unchanged non-empty set re-proves via the prior fact).
   Wire/store stay ADDITIVE (goldens updated; unknown-kind tolerance
   law re-proved).
5. **Compaction honesty**: the same composed prompt (with instructions)
   feeds `post_compaction_system_prompt` and the W7 fit check. Token
   accounting already counts system_prompt — pin that instructions are
   included in the estimated footprint.
6. **Recovery**: a recovered mid-flight turn re-loads via the SAME
   pinning discipline it uses today for provider resolution; if the
   journaled fact for the turn exists, recovery composes from the
   journaled digests' semantics (re-read files; if digests differ, the
   re-read wins — journal a fresh fact; the law is "one pinned turn,
   one prompt", not cross-crash bit-stability).

## Laws (minimum)

- empty_walk_composes_byte_identical_v1_prompt (no files → today's
  prompt except the version line policy you choose; pin explicitly).
- nearest_instructions_compose_last_and_win / haider_md_beats_agents_md
  _within_a_directory.
- per_file_and_total_caps_truncate_at_utf8_boundary_with_marker.
- upward_walk_stops_at_root_and_never_reads_through_symlinked_parents.
- one_pinned_logical_turn_sees_one_instruction_snapshot (retry/round
  sweep).
- edits_take_effect_next_logical_turn.
- loaded_fact_is_journaled_with_digests_and_replays_omitted.
- unknown_payload_kind_tolerance_still_holds (golden).
- footprint_estimate_counts_instruction_bytes.
- compaction_fit_check_includes_instruction_block.
- loader_is_not_a_broker_effect (no effect journal rows from a turn
  whose only novelty is instructions).

Standing lane laws: tests never inline; mutation-notes doc with
RUNTIME failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D
warnings; additive protocol only; ledger update; no haider-tui; no
Cargo.lock; no version bumps; leave changes uncommitted; run no git
commands. Use up to 3 research subagents and 2 verify subagents.
Finish with a summary of files changed and tests added.
