# W3b1 — haider-daemon lifecycle core (singleton, endpoint, ready gate, drain)

Binding sources (read FIRST, they win over this brief): docs/research/
d1-daemon-research-report.md §6 (plumbing), §7 (crash recovery / C4a seam),
§8 (crate decomposition), RECOMMENDATIONS 1, 2, 3, 4, 16, 17, 18, 22.
Prior art in-tree: haider-store's profile lock + worker_generation machinery
(read crates/haider-store — the lifetime lock IS the singleton authority);
haider-core's EffectBroker + startup reconciliation seam (C4a) for the
reconcile-before-ready gate; crates/haider-rpc for Welcome/LifecyclePhase.

## Scope

NEW crate `crates/haider-daemon` (runtime library — R18: singleton, connection
lifecycle, recovery, shutdown live here) + NEW thin binary crate
`crates/haider-daemond` (bin name `haiderd`) that only parses args, builds
config, and calls the library. NO session hub, NO attach/replay barrier, NO
menu routing — those are W3b2. This lane delivers a daemon that: starts
exactly once per profile, owns a UDS endpoint safely, refuses to serve before
recovery completes, answers Hello/Welcome + Ping/Pong (using haider-rpc), and
drains honestly on shutdown. Connections beyond handshake+ping return
ResponseBody::Error { code: "draining" | "not_found" ... } stubs documented as
W3b2 seams.

## Deliverables

1. Singleton (R1): acquire haider-store's lifetime lock BEFORE any socket
   cleanup or store open; hold through shutdown; release LAST. Socket/PID files
   are diagnostics only, never authority. Losers exit with a typed
   AlreadyRunning error carrying the incumbent's diagnostics.
2. Endpoint (R2): filesystem UDS at a fixed-length profile-derived name under
   a 0700 per-user runtime dir; socket chmod 0600; peer-UID check where the
   platform provides it (macOS: getpeereid via std/libc); NO Linux abstract
   sockets.
3. Stale cleanup (R3): probe first; unlink only after ECONNREFUSED AND an
   lstat-verified owner/dir match; record device+inode after bind; the cleanup
   guard removes ONLY that exact socket identity. Named test: an old daemon
   must not delete its successor's socket (R22 case).
4. Ready gate (R16): open store under the singleton → durably bump
   worker_generation → run C4a reconciliation for every dispatched-without-
   terminal effect (EffectOutcomeUnknown exactly once where truth is
   unknowable — reuse haider-core's machinery; if a seam is missing there, add
   it ADDITIVELY) → only then bind listeners and advertise
   LifecyclePhase::Ready in Welcome. Startup phases surface as typed states
   (Starting/Recovering/Ready/Draining/Failed) queryable by the readiness
   handshake (R4's daemon half; the CLI connect-spawn-connect half is W3c).
5. Drain barrier (R17): first shutdown signal → Draining: reject new mutations,
   send ServerDraining { reason, instance_id, daemon_generation,
   deadline_unix_ms } to every connection, bounded completion window, flush
   store, close connections, remove the exact owned socket, release lock LAST.
   Second signal → immediate termination path (recovery is the next
   generation's job — document, don't handle).
6. Connection layer: accept loop with per-connection tasks; handshake =
   Hello → negotiate() (haider-rpc) → Welcome carrying instance_id,
   daemon_generation, frame_limit, lifecycle phase; enforce the client's
   max_receive_frame on outbound; Ping/Pong; bounded per-connection outbound
   queue (R12's mechanism only — no session fan-out yet); UDS framing via
   haider-rpc's uds_codec with its DecodeBatch chunk-invariance semantics.
7. Tests (R19/R22 named cases for THIS scope): simultaneous-start (N
   processes, one wins, losers exit clean), stale-PID-reuse, cold-start
   socket-missing, successor-socket-deletion, failed-listener-startup (bind
   error → Failed phase + lock released), abrupt-death (kill -9 → next start
   recovers and serves), handshake version-mismatch rejection, oversize frame
   at the connection layer, drain-notifies-connections, second-signal
   termination. Use real UDS in tests (tempdir runtime dir); no sleeps as
   synchronization — poll readiness states.

## Rules (workspace law)
- Tests in tests/, count only up, clippy -D warnings, no unwrap/expect outside
  tests, fmt clean, baseline regenerate, honest commit message w/ test counts.
- tokio may enter haider-daemon (it is the runtime library) — pin the feature
  set to what's needed; haiderd stays thin (< 100 lines).
- Do not modify haider-rpc except ADDITIVE needs discovered here (document
  any in the commit); do not touch haider-tui.

## Gate
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
&& cargo fmt --all -- --check && cargo run -p xtask -- test-count
