# Patch brief W1/B1 — haider-store: durability port + journal + CAS

Own crates/haider-store ONLY (+ its tests/). Read crates/haider-protocol (frozen contracts —
EventEnvelope, RawEnvelope, ids) and CONVENTIONS.md first. Deps you may add: rusqlite
(bundled), blake3, serde/serde_json (workspace), tempfile (dev-dep). Update workspace
Cargo.toml [workspace.dependencies] for new deps.

B1a — THE DURABILITY PORT (do first, it is the contract):
- trait EventStore: append(envelopes) -> committed seq range (atomic, allocates monotonic
  per-session seq at commit); read(session, since_seq, limit) -> Vec<RawEnvelope>;
  latest_seq(session). Law: an event is TRUE only after append returns; publish-after-commit
  is the caller's duty; replay MUST reproduce identical envelopes byte-for-byte (JSON).
- trait Cas: put(bytes) -> ArtifactRef (blake3 hex, "blake3:<hex>"); get(ref) -> bytes;
  verify(ref) -> bool. Atomic write (temp+rename), fsync, corruption detected on get.

B1 — implementation:
- SQLite (WAL, busy_timeout, foreign keys) at <root>/store.sqlite: events table (session_id,
  seq, envelope_json, event_id UNIQUE, committed_at_ms) w/ (session_id, seq) PK; sessions
  table (id, created_at_ms, meta_json); migrations: schema_version pragma + registry of
  numbered migrations, idempotent, tested for fresh + re-open.
- CAS at <root>/cas/<2-hex>/<hex>: content-addressed, dedup by hash.
- Profile lock: <root>/lock file via O_EXCL-equivalent (rusqlite exclusive or flock) — second
  opener gets StoreLocked error (protocol ErrorCode).
- Recovery: journal_replay(session) folds envelopes; corrupted trailing JSON line policy =
  SQLite makes this moot (row-atomic) — test re-open after simulated kill (drop conn mid-txn).
Tests (tests/store_tests.rs — the oracle): append/read round-trip byte-identical; seq
monotonic + gap-free per session; concurrent-ish appends (two conns) serialize; CAS
put/get/verify + corruption injection; lock exclusivity; migration fresh+reopen; RawEnvelope
forward-compat (insert an envelope with unknown payload type string — reads back intact).
Gate: cargo test -p haider-store, clippy -D warnings, fmt, xtask check (baseline will RISE —
run `cargo run -p xtask -- test-count --update` and commit the new baseline). <10k LOC/file.
Leave changes uncommitted.
