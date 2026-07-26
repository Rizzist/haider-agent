# Lane brief W2b/C2 — tools completion: request_input, shell, process control

DRAFT — launches after the W2a merge tags v0.0.6 (needs C4a's EffectBroker + ledger in
main). Own crates/haider-tools (+ actor wiring in haider-core where noted).

Scope
1. `request_input` tool: the model asks the user a typed question → emits a protocol
   Menu (MenuKind::Question/Choice, options enumerated SERVER-side — clients render,
   never invent), run enters RunState::InputRequired{menu}; the answer returns as the
   tool result. Non-permission menus do NOT go through the permission broker, but the
   open-menu/answer lifecycle must journal (MenuOpened/MenuAnswered payloads).
2. `process_exec` behind the broker: effect class process, args_digest over
   {command, cwd, env_allowlist}; 4-phase journal; streamed output as CommandOutput
   deltas (BYTES, base64 — never lossy at the wire); bounded transcript tail is the
   TUI's concern, the store keeps full output via CAS artifact when it exceeds the
   inline cap. exit_code + ToolStatus in the Completed item. Cancellation kills the
   process group (SIGTERM → grace → SIGKILL) and journals Cancelled as an OUTCOME
   (never a failure), reusing the C4a terminalization claim.
3. `process_control`: send-signal/stdin-write/kill for a live process_exec by call_id —
   each a broker-mediated effect bound to the original effect id.
4. Builtins + `!` escape: the composer's `!<cmd>` runs a user-initiated shell (distinct
   authorization source: user-typed = pre-authorized, journaled as such — the
   permission prompt is for MODEL-initiated effects). Builtin commands (cd, env-view)
   resolve harness-side without a subprocess.
5. FakeProvider gains EmitRequestInput and process-flavored steps as needed so the
   actor's InputRequired path has a deterministic oracle.

Laws
- Every effectful path goes through EffectBroker (single mutation door); the
  terminalization claim from C4a.4 is the ONLY way to journal a terminal phase.
- Process output is bytes end-to-end (codex lesson); UTF-8 assembly only at display.
- Tests in tests/ files; exact-count assertions for journal phases; kill-grace timing
  behind a test clock, no sleeps over 100ms in CI.

Gate: cargo test --workspace, clippy -D warnings (all targets), fmt --check,
xtask test-count --update, git diff --check. Leave uncommitted.

## Launch addenda (2026-07-26, lane open)
- The C4a terminalization machinery (claim_terminal, broker-owned finalizers, close()
  drain/sweep) is NOW IN MAIN — process_exec/process_control MUST journal through it;
  do not invent a parallel path. Read crates/haider-tools/src/broker.rs contract headers
  first.
- Actor wiring for InputRequired lives in haider-core: keep core edits minimal and
  surgical (RunState::InputRequired already exists in the protocol; the actor needs the
  menu round-trip seam + FakeStep support).
- Kill-grace timing behind a test clock: tokio::time::pause-style tests, no wall sleeps.
