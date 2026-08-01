# B4a — attachments live, core half: byte ingress, validation, caps, headless

AUTHORITY: docs/research/b4-attachments-research.md (read WHOLE,
first). Line numbers approximate — re-locate every seam. The TUI half
(B4b: /attach, real paste pill) is a SEPARATE lane — NO haider-tui
changes here.

## Scope (rpc/daemon/store/provider/core/client/cli)

1. **artifact.put RPC**: upload bytes into the daemon CAS. Content-
   addressed → idempotent (re-put of existing bytes succeeds cheaply);
   NO command receipt. Respect the negotiated frame_limit: chunked
   sub-protocol or a documented per-call byte bound — your choice, but
   a client must be able to land a 5 MiB image without renegotiating
   limits. Response returns the verified `ArtifactRef` + byte count.
   Enforce a hard per-call/total cap (8 MiB pre-base64) with a typed
   error. Feature bit `artifact_put_v1`. Wire golden updated
   (ADDITIVE).
2. **Acceptance-time validation** in turn_submit: every attachment
   artifact must EXIST in the CAS (dangling ref → typed error at
   submit, not a dead run later); mime allowlist for Image
   (jpeg/png/gif/webp); caps: ≤5 MiB per attachment, ≤5 attachments
   and ≤16 MiB total per turn. Typed, remediable errors.
3. **Vision gating**: at acceptance (or turn start — pick the seam
   that gives the clearest typed error before any provider spend),
   image attachments against a provider whose
   `capabilities().vision` is Unsupported → typed local refusal
   naming the provider. Fake provider gains a vision-Native switch for
   fixtures if needed (additive).
4. **Image-aware footprint**: estimate_provider_request_input_tokens
   must stop charging images at base64-bytes/4 — count a fixed
   per-image vision estimate (document the constant; ~1.6k tokens
   default, dimension-scaled if width/height present) while PastedText
   (already inlined as text) keeps byte-based counting. Pin that a
   5 MiB image no longer detonates the W7 threshold.
5. **Compactor policy**: summarization TurnRequests EXCLUDE image
   attachments (text-only summary; kills the re-inflation spiral).
   Post-compaction ancestry keeps the attachment REFS intact on
   surviving nodes per the existing substitution law.
6. **Headless**: repeatable `--attach <path>` on `haider run`: sniff
   mime from magic bytes (jpeg/png/gif/webp; unknown → typed error),
   read + artifact.put BEFORE turn.submit, blocks ride the immutable
   submit body from the first send (durable command identity).
   haider.run.v1 JSON gains an additive `attachments` count/refs
   field. Never bypass the RPC with direct FileCas writes.

## Laws (minimum)

- artifact_put_roundtrip_is_content_addressed_and_idempotent.
- oversized_put_and_oversized_turn_are_typed_errors.
- dangling_ref_rejected_at_submit_not_at_run.
- mime_allowlist_enforced_at_acceptance.
- vision_unsupported_provider_refuses_locally_with_typed_error.
- image_footprint_uses_fixed_vision_estimate_not_base64_length.
- compaction_summary_request_carries_no_image_attachments (and refs
  survive substitution).
- headless_attach_uploads_then_submits_with_durable_identity (retry
  resends identical body).
- run_json_reports_attachments_additively (golden).
- unknown_payload_kind_tolerance_still_holds.

Standing lane laws: tests never inline; mutation-notes doc with
RUNTIME failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D
warnings; additive protocol only; ledger update; NO haider-tui; no
Cargo.lock; no version bumps; leave changes uncommitted; run no git
commands. Use up to 3 research subagents and 2 verify subagents.
Finish with a summary of files changed and tests added.
