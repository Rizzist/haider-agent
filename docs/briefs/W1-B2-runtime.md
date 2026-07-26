# Patch brief W1/B2 — haider-core runtime skeleton + fake provider + haider-provider trait

Own crates/haider-core, crates/haider-provider, and (for B4) crates/haider-cli main.rs ONLY
(+ their tests/). Read crates/haider-protocol (frozen) and CONVENTIONS.md. Deps: tokio
(workspace add: rt-multi-thread, macros, sync, time, process), async-trait if needed.

haider-provider:
- trait Provider (async): capabilities() -> CapabilityDoc; stream_turn(request) ->
  event stream (protocol::provider::StreamEvent via tokio mpsc or async_stream).
  TurnRequest { messages: Vec<Block-based>, model, max_tokens } — minimal for now.
- FakeProvider: fixture-driven — reads a script (Vec<FakeStep>: EmitText, EmitToolCall,
  SplitUtf8 (emit a multi-byte char across two deltas), MalformedFrame, Delay(ms), EmitUsage,
  Finish(reason), Hang) from JSON; deterministic; used by ALL runtime tests.
haider-core:
- HarnessActor: owns a session's run loop v0: accepts SubmitTurn { text } → drives provider
  stream → converts StreamEvents to protocol ItemEvents (item lifecycle: started/delta/
  completed for AgentMessage; tool calls surfaced but NOT executed yet) → commits envelopes
  through a StoreHandle trait (define it here mirroring haider-store's EventStore so B1/B2
  merge cleanly — do NOT depend on haider-store crate yet) → publishes to subscribers
  (tokio broadcast). RunState transitions: Queued→Thinking→Streaming→Done, projected and
  committed as RunState envelopes. Cancellation: a CancelToken aborts the stream cleanly →
  Cancelled (FinishReason::Cancelled semantics — an outcome, never an error).
- Worker generation + authority epoch stamped on every envelope (u64 params for now).
B4 — thin CLI: `haider run --jsonl "<prompt>"` uses FakeProvider + in-memory StoreHandle to
stream envelopes as JSONL to stdout (LF-only framing law); exit 0 on Done, sysexits-style 65
on Errored. Keep --version/self-test intact; extend self-test with a fake-provider turn check.
Tests (tests/ dirs — the oracle): full turn vs fake provider produces exact expected envelope
sequence (golden-ish, ignore ids/ts); split-UTF8 reassembly correct; malformed frame → Errored
with typed error (never panic); cancellation mid-stream → Cancelled + no further events; hang
+ cancel works; JSONL output parses line-by-line as RawEnvelope. Gate: full workspace gate;
update test-count baseline. Leave changes uncommitted.
