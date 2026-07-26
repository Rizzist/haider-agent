# W3a — haider-rpc wire crate (frames, codecs, conformance)

Source of truth: docs/research/d1-daemon-research-report.md §5 (proposed protocol),
§6 (plumbing), RECOMMENDATIONS 6, 7, 9, 19. Read those sections FIRST — this brief
binds you to them; where this brief is terser than the report, the report wins.

## Scope

Pure wire crate. NO session policy, NO daemon orchestration, NO transport listeners
(R18 boundary): frames, codecs, version negotiation types, and a parameterized
conformance harness. `haider-daemon` (W3b) will consume this crate; the daemon does
not exist yet — do not invent it here.

## Deliverables

1. `WireFrame` — one versioned logical union (serde, adjacently or internally tagged;
   pick one, document why, and add serde round-trip + JSON-shape golden tests):
   - `Hello { protocol_min, protocol_max, client_kind, capabilities_requested }`
   - `Welcome { protocol, instance_id, daemon_generation, frame_limit, lifecycle_phase,
     capabilities_granted }`
   - `Request { request_id, body: RequestBody }` / `Response { request_id, body: ResponseBody }`
   - `Event { session_id, envelope: RawEnvelope }` (re-use haider-protocol's RawEnvelope;
     `RawEnvelope.seq` is THE cursor — no parallel event id, R9)
   - `AttachCaughtUp { attachment_id, high_water_seq }`
   - `MenuAnswer { command_id, session_id, menu_id, request_seq, worker_generation, option }`
     (the durable-CAS command shape from R13 — wire shape only here; arbitration is daemon work)
   - `Lagged { attachment_id, resume_after_seq }`
   - `ServerDraining { deadline_ms }`
   - `ProtocolError { code, message, fatal }`
   NOT JSON-RPC; do not claim or imitate JSON-RPC framing (R6).
2. `RequestBody`/`ResponseBody` for the v0.1 method set (R8): `SessionList { page, page_size }`
   (paginated), `SessionRead { session_id, range }` (non-subscribing), `SessionAttach
   { session_id, after_seq, mode: view|control }` → `{ attachment_id, attach_state }`,
   `SessionDetach { attachment_id }`. Include a `Ping`/`Pong` pair for liveness.
   Every enum gets `#[non_exhaustive]` or a reserved/unknown arm so old clients
   tolerate new frames (mirror haider-protocol's tolerance discipline).
3. Codecs (R7): one JSON object per WS text message (`ws_codec`: encode/decode a
   WireFrame ↔ String); UDS = 4-byte big-endian length prefix + UTF-8 JSON
   (`uds_codec`: streaming decoder struct that accepts arbitrary byte chunks,
   enforces the frame limit BEFORE allocating the body buffer, and yields frames as
   they complete). Same JSON body bytes on both transports — one serializer, two framers.
   Oversize frame → typed error naming the limit; never a panic, never a partial alloc.
4. Version/capability negotiation helpers: `negotiate(client: &Hello, server_range)`
   → `Result<Negotiated, ProtocolError-shape>`; capability set = `view | control`
   modeled now (R14) even though one control token ships first.
5. Conformance harness (R19 skeleton, in `tests/`): a parameterized suite that runs
   the SAME decoded transcript (a Vec of WireFrames covering every variant) through
   both codecs and asserts byte-level determinism + round-trip identity; UDS-specific
   cases: fragmentation (1-byte drip feed), coalesced frames (two frames in one chunk),
   split length prefix, oversize rejection mid-stream leaves the decoder recoverable
   or poisoned (pick one, document, test it), empty frame, max-exact frame.
6. Docs: crate-level doc explaining the R9 seq-only-cursor law, the R18 boundary,
   and what W3b will build on top. Reserved fields commented where the daemon will
   fill semantics.

## Rules (workspace law — violations = NO_SHIP)
- Tests in `tests/`, never inline. Test count only goes up (xtask test-count).
- clippy -D warnings; no unwrap/expect outside tests; fmt clean.
- serde tolerance: unknown fields ignored on decode where safety allows; document
  where strictness is deliberate (length prefix, frame limit).
- Do NOT touch other crates except workspace Cargo.toml dep wiring if needed.
- Baseline regenerate at the end; honest commit message with test counts.

## Gate
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings &&
cargo fmt --all -- --check && cargo run -p xtask -- test-count
