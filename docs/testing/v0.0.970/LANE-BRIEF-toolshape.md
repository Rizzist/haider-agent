# Lane toolshape — declared truncation marker + effect path in tool results (v0.0.970, S/M)  Branch lane-970-toolshape.
Read docs/testing/v0.0.970/LANE-COMMON.md FIRST. AHRB finding (2026-09-03): rows 60/61 reverse-engineer (a) large-tool-output truncation and (b) which
file a fixture write touched. DELIVER: (a) when tool output is truncated, the tool result carries a typed marker `{truncated: true, original_bytes,
payload_bytes, sha256}` (sha256 of the ORIGINAL bytes; payload keeps today's prefix/suffix policy) — additive in the tool result payload and in the durable
event; (b) file-effect tool results (write/edit/create) declare `effects: [{kind, path (workspace-relative + absolute), bytes}]` matching the workspace
receipt. Both documented in docs/jsonl-run-contract-v1.md + event-schema-changelog (additive). Tests: golden tool results for a 1 MiB stdout truncation
and a fixture write (byte-identical otherwise); replay parity unchanged; the 969 JSONL goldens stay green or are re-blessed with the documented additive
fields only. `bash run.sh test` green; docs/testing/v0.0.970/toolshape.md. LAST line: SHIP or NO_SHIP.

LOCKED LITERALS (agreed with the AHRB adapter 2026-09-03 — implement EXACTLY these; they are pinned externally):
- Truncation marker (text-facing): when a tool's output is truncated, the payload text ends with ONE line, on its own line:
  `[haider:truncated truncated=true original_bytes=<uint> payload_bytes=<uint> sha256=<64 lowercase hex of the ORIGINAL bytes>]`
  (no padding, no payload bytes inside the marker). Typed mirror in the durable tool-result payload JSON at pointer `/truncation`:
  {"truncated":true,"original_bytes":<uint>,"payload_bytes":<uint>,"sha256":"<hex64>"}; absent (not null) when nothing was truncated.
- Effect paths (typed): tool results that write/create/edit/delete files carry `/effects` = array of
  {"kind":"write|create|edit|delete","name":"<basename>","path":"<workspace-relative>","absolute_path":"<abs>","bytes":<uint>} in effect order;
  first effect at `/effects/0/path`, `/effects/0/name`. Matches the workspace receipt entries one-to-one.
