# G4a — local OSS providers: mutation notes

Seven single-anchor mutations EXECUTED against committed trees (work
committed first at a6a2ed6/eaf903f/c1f8b1b/425a836; each mutation applied
to production code only, the ONE named law run with `running 1 test`
observed, the failure recorded verbatim, the mutation reverted via
`git checkout`, and the law re-run green). Six killed; one survived with
an equivalence analysis (M3). Coverage includes the origin matrix in BOTH
directions and three decoder tolerances.

## Executed kills

### M1 — origin matrix, WRONGLY-ALLOWED direction (killed)

- Anchor: `rfc1918_private` (openai.rs) V4 arm →
  `address.is_private() || address.is_link_local()` — the TrustedLan
  exemption swallows the cloud-metadata range.
- Observer: `openai_provider_tests::lk3_custom_origin_matrix_allows_rfc1918_and_keeps_metadata_and_public_http_blocked`.
- Observed: `running 1 test` → FAILED, panic at
  openai_provider_tests.rs:555 (`expect_err("origin must stay refused
  under TrustedLan")` — `http://169.254.169.254` constructed).
- Reverted; green.

### M2 — origin matrix, WRONGLY-BLOCKED direction (killed)

- Anchor: `blocked_credential_target_with_policy` TrustedLan arm →
  `blocked_credential_target(address)` (TrustedLan behaves Strict).
- Observer: same LK3 literal-matrix law.
- Observed: `running 1 test` → FAILED, panic at
  openai_provider_tests.rs:538: ``custom LAN origin
  `http://192.168.1.8:11434` refused: InvalidRequest: OpenAI-compatible
  base_url must not target a link-local, multicast, or special-use IP
  address``.
- Reverted; green.

### M3 — SSE comment skip deleted (SURVIVED — equivalent mutant)

- Anchor: removed `if line.starts_with(':') { return None; }` from
  `SseFramer::accept_line`.
- Observer: `lk5_chat_stream_ignores_sse_comment_ping_lines` — `running 1
  test` → ok (survived).
- Analysis: a comment line `": ping"` that reaches the field dispatch
  splits as field `""` / value `"ping"`, and `""` matches neither
  `"event"` nor `"data"` — the `_ => {}` fallthrough discards it. The
  tolerance is doubly enforced (explicit guard + field dispatch); the
  guard alone is redundant defense-in-depth, so its deletion is an
  equivalent mutant, not a coverage gap. Killing LK5 requires breaking
  the field dispatch itself, which the unknown-SSE-field discard shares
  with the `id`/`retry` handling — left in place as safe code. Recorded,
  reverted.

### M4 — LK4 tolerance deleted: EOF always errors (killed)

- Anchor: `ChatDecoder::finish` — removed the
  `if let Some(reason) = self.finish_reason` tolerant branch; EOF without
  `[DONE]` always emits the malformed-stream error.
- Observer: `lk4_chat_stream_missing_done_sentinel_completes_on_eof`.
- Observed: `running 1 test` → FAILED, panic at
  openai_provider_tests.rs:117 (clean `TextDelta+Finish{EndTurn}`
  sequence became an error item).
- Reverted; green.

### M5 — LK7 tolerance deleted: missing tool id rejected (killed)

- Anchor: `ChatDecoder::tool_delta` vacant-entry arm — `None` id restored
  to `return Err(malformed("… started without an id"))`.
- Observer: `lk7_chat_tool_calls_without_ids_synthesize_stable_per_index_ids`.
- Observed: `running 1 test` → FAILED, panic at
  openai_provider_tests.rs:210 (the expected synthesized-id event
  sequence collapsed to one error item).
- Reverted; green.

### M6 — LK8 upgrade deleted: "stop" drops open tool calls (killed)

- Anchor: `ChatDecoder::finish_events` — removed the
  `EndTurn && !open_calls.is_empty() → ToolUse` upgrade.
- Observer: `lk8_chat_finish_stop_with_tool_calls_still_completes_the_calls`.
- Observed: `running 1 test` → FAILED, panic at
  openai_provider_tests.rs:262 (tool events dropped, Finish{EndTurn}
  instead of the completed calls + Finish{ToolUse}).
- Reverted; green.

### M7 — keyless resolution fallback deleted (killed)

- Anchor: `AccountsProviderFactory::resolve_provider` — removed the
  `CredentialMissing → keyless_account` arm (accounts.rs).
- Observer: `haider-daemon accounts_tests::lk1_keyless_profile_resolves_placeholder_and_stored_key_wins`.
- Observed: `running 1 test` → FAILED, panic at accounts_tests.rs:476
  (`expect("keyless dispatch")` — resolution reported CredentialMissing).
- Reverted; green.

### M8 — TUI preset origin typo (killed)

- Anchor: `open_ollama_preset` origin → `http://127.0.0.1:1143/v1`.
- Observer: `g4a_local_oss_presets_tests::ollama_preset_prefills_the_local_origin`.
- Observed: `running 1 test` → FAILED, assertion `left == right`:
  left `"http://127.0.0.1:1143/v1"`, right `"http://127.0.0.1:11434/v1"`.
- Reverted; green.

## Documented (unexecuted) mutation checks

Each new law's doc-comment names further single-anchor mutations with
their expected RUNTIME failures: the Strict-routing of `new_custom`
(LK3), dropping the RFC1918 arm from the catalog backstop (LK3 catalog),
requiring usage before Finish (LK6), rejecting unknown chunk fields
(LK9), deleting the factory keyless arm — observable since the
key-requiring arm now excludes empty-auth profiles (LK1), refusing
auth-None customs in `catalog_source` (LK2), blanking the credential in
`authorization_header` (LK1 golden), dropping the keyless branch from
`custom_add_committed` (LK10), and dropping the `keyless_local` row-hint
branch in `render_providers` (LK10).

## Review of record (coordinator, executed post-lane)

Read the branch diff (origin policy, guard, keyless arm, decoder, presets).
Verdicts:

1. LAN loosening is REBINDING-SAFE by construction: CompatibleOriginGuard
   resolves once, validates the RESOLVED addresses against the policy
   (validate_resolved_compatible_origin), caches via OnceCell, and the
   pinned reqwest resolver refuses unexpected hosts — connect-time targets
   are exactly the validated set. LK3's hostname-resolution matrix pins
   both directions; wrongly-allowed AND wrongly-blocked mutations both
   executed and killed.
2. M3 equivalent-mutant claim VERIFIED in source: SSE comment lines are
   discarded by the `starts_with(':')` early return AND independently by
   the empty-field `_ => {}` dispatch arm. Genuine equivalence, honestly
   documented; LK5 observes the behavior either way.
3. Keyless scoping observed (lk1_keyless_fallback_stays_scoped...), and
   the lane's re-guard of the generic arm specifically to make the new arm
   mutation-observable is the right instinct.

No unobserved gate found. Campaign ACCEPTED.
