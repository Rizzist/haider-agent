# W3b2 — session hub: attach/replay barrier, backpressure, MenuAnswer CAS

Binding sources (read FIRST; they win over this brief): docs/research/
d1-daemon-research-report.md **§5.4, §5.5, §5.6, §5.7** (read these verbatim —
§5.5 states the flow as 8 numbered steps and two invariants; implement exactly
that) plus RECOMMENDATIONS 8, 9, 10, 11, 12, 13, 14.
In-tree prerequisites (already shipped — build ON them, do not re-model):
- `crates/haider-rpc` — SessionList/SessionRead/SessionAttach/SessionDetach
  bodies, `Event { attachment_id, session_id, envelope }`, `AttachCaughtUp`,
  `Lagged { attachment_id, last_queued_seq }` (informational only — the client's
  applied cursor is the resume authority, R9), `MenuAnswer { command_id,
  session_id, menu_id, request_seq, worker_generation, option_key, option_index,
  input }`, `ResponseBody::Error { code, message, retryable, data }` with
  `ErrorData::{CursorAhead{requested,head}, AlreadyResolved{resolution_seq}}`.
- `crates/haider-daemon` (W3b1) — singleton, endpoint, phase machine, drain
  barrier, connection layer with bounded per-connection outbound queue +
  queued-byte budget and RESERVED drain capacity. The `Request` / `MenuAnswer`
  arms currently return documented stubs (`enqueue_stub`) — those stub sites
  are exactly your seams.
- `crates/haider-store` — `RawEnvelope.seq` is the ONLY cursor; paged reads
  keyed on `(session_id, seq)`.

## Scope

The session hub: per-session actors, attachment lifecycle, replay/live barrier,
fair multi-session scheduling, lag recovery, and durable menu arbitration.
NOT in scope: the CLI `haider attach` / auto-start / `--standalone` (W3c), the
localhost WebSocket + webview auth (W3d), and the recovery-projection
optimization slice (ledgered in OPTIMIZATIONS.md — do not casually cache).

## Deliverables

1. **Session actor** (R10, §5.5 invariants). One actor per live session with a
   serialized command loop. The two invariants are load-bearing and must be
   provable by test:
   - `persist event before publish`
   - `register receiver + observe committed head H in the same actor order as
     append/publish`
   Registering an attachment and capturing `H` happen in ONE serialized step —
   never as two awaits with a yield between them.
2. **Attachment lifecycle** (R8, §5.4). `SessionList` (paginated, opaque cursor,
   fixed `session_id` ascending order), `SessionRead` (non-subscribing),
   `SessionAttach { session_id, after_seq, mode }` → unique `attachment_id` +
   `AttachState`, `SessionDetach`. One connection MAY hold several attachments
   (GUI dashboard); attaching/detaching must never change session authority or
   worker ownership. Presence is connection/attachment metadata, NOT a durable
   session event.
3. **Replay → caught-up → live** (R10, §5.5 steps 3-7). Replay task reads store
   pages for `(after_seq, H]` in ascending seq; `AttachCaughtUp { through_seq: H }`;
   drain buffered `> H` dropping duplicates by seq; then live. Deterministic
   tests must force an append at EVERY boundary: before registration, between
   registration and H capture (must be impossible by construction — assert the
   ordering), during replay, exactly at H, immediately after H, during the
   buffered drain.
4. **Cursor semantics** (R9, R11). `after_seq` is the client's greatest FULLY
   APPLIED seq. `after_seq > H` → `ResponseBody::Error` code `cursor_ahead`
   carrying `ErrorData::CursorAhead { requested, head }`. At-least-once
   delivery: duplicates by seq are the client's to ignore; a gap means the
   client reattaches after its applied cursor. Do NOT add any competing
   ephemeral counter, notification offset, or snapshot generation.
5. **Backpressure + lag recovery** (R12, §5.6). A session actor must NEVER block
   on a socket. Publication enqueues into the bounded connection writer. On
   overflow: emit `Lagged { attachment_id, last_queued_seq }` if that frame
   itself can be queued, then detach that attachment (or close the connection)
   and let it resume from the store after its applied cursor — the store is the
   lag buffer; no unbounded per-client queue, ever. Fair scheduling across
   several attachments on one connection: a hot session must not starve others
   (round-robin or equivalent — document the policy and test starvation).
6. **MenuAnswer durable CAS** (R13, §5.7). Any control-capable attachment may
   answer; exactly one answer becomes authoritative. Validate: capability
   `control`; attachment is attached to that session (unless policy explicitly
   permits a controller without a viewport — document which); freshness against
   `request_seq` and `worker_generation` (stale generation → rejected, fenced).
   Atomically append ONE resolution. First committed answer wins; losers get
   `ResponseBody::Error` code `already_resolved` carrying
   `ErrorData::AlreadyResolved { resolution_seq }`. ALL attachments learn the
   outcome through the event stream — never through a private reply only.
   Pending menus survive with the session: a client attaching after the prompt
   was raised learns of it through `RawEnvelope` replay (no separate pending
   cache). A daemon restart must NOT resend a protected effect because an
   in-memory waiter vanished.
7. **Capability enforcement** (R14). Centralize method authorization in one
   place; `view` may not answer menus or mutate. `capability_denied` is a
   correlated error.
8. **Drain integration**: attachments are notified via the W3b1 drain barrier;
   in-flight replay tasks must be cancellable and must not hold the store open
   past the drain deadline.

## Tests (R19/R20 semantic seams — the acceptance matrix for this lane)
Replay/live interleavings at every boundary (above); many concurrent cursors on
one session; slow client → Lagged → store-resume continuity (no lost or
duplicated seq across the transition); N-way MenuAnswer race (N tasks, exactly
one durable winner, N-1 `already_resolved` with the winner's `resolution_seq`);
stale `worker_generation` answer rejected; lost response after commit (client
never sees the reply but the resolution is durable and arrives via the stream);
attach-after-menu-raised sees the pending menu through replay; `cursor_ahead`
shape; multi-attachment fairness/starvation; detach mid-replay leaks nothing;
drain during replay. Assert contiguous event histories and generation fencing.

## Rules (workspace law)
Tests in tests/ (never inline); test count only up; clippy -D warnings; no
unwrap/expect outside tests; fmt clean; regenerate baseline; honest commit
message with accurate test counts. Additive-only changes to haider-rpc and
haider-protocol (document any in the commit). Do not touch haider-tui.
No sleeps as synchronization — poll/await real state.

## Gate
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
&& cargo fmt --all -- --check && cargo run -p xtask -- test-count
