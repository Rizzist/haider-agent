# W3b1 review round 1 — NO_SHIP

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-27
- Frozen SHA: 1183fb4 (scope 1067e84..1183fb4: lane 58b3b62 + clean-code 2433ede + rider 1183fb4)
- R-compliance: R1/R2/R4/R12/R16/R18 COMPLY; R3, R17, R22 VIOLATE.
- Full log: ~/haider-run/w3b1-review-r1.log

## Findings

P1
1. **Drain cancellation DETACHES the blocked writer** (connection.rs:44/174, runtime.rs:331):
   WriterTask::finish removes its JoinHandle before awaiting it. If a client never reads while
   an ordinary large frame is mid-write_all, the reserved drain frame waits behind that write;
   at the drain timeout, aborting `serve` drops the now-bare handle and DETACHES the writer
   instead of aborting it → ServerDraining never delivered, and the socket/task/payload can
   survive endpoint cleanup AND singleton-lock release. (The reserved slot alone doesn't save
   us — the writer is stuck in write_all, not waiting for a slot.)
2. **Deadline + second signal do not cover finalization** (runtime.rs:331/352/364,
   event_store.rs:199): after the last shutdown-watch check, flush/cleanup/close run with no
   deadline and no second-signal select. A SQLite reader can hold wal_checkpoint(TRUNCATE)
   through its 5s busy timeout, keeping socket + profile lock past the advertised deadline; a
   second signal in that tail is ignored and shutdown can still report Graceful. Pre-listener
   shutdown has the same unbounded flush tail.
3. **R3/R22 pathname replacement still TOCTOU** (endpoint.rs:76/123/216): both stale and owned
   cleanup do symlink_metadata → pathname remove_file. A same-UID process replacing the node
   between those calls has its replacement/successor deleted; replacement between bind and
   post-bind lstat can make the daemon record the SUCCESSOR's identity as its own. The existing
   R22 test replaces before the identity check and misses both windows.

P2
4. Drain notice can fail ENCODING (client frame limit fits Welcome but not a long public
   shutdown reason) before using the reserved slot; runtime discards the result and may still
   report Graceful. (connection.rs:197, runtime.rs:334)
5. **W3b2 blocker**: Response requires RequestId but top-level MenuAnswer carries only
   CommandId → a CAS loser cannot receive the prescribed correlated `already_resolved` response
   without changing the wire model. (frame.rs:439/466, connection.rs:307)
6. Negotiated capabilities discarded — connection keeps only `handshaken: bool`; W3b2 cannot
   distinguish view vs control without remodeling the handshake seam. (connection.rs:191/355)
7. Pre-handshake peers permanently exhaust admission: max_connections same-UID peers can connect
   and never send Hello, holding every permit forever. Needs a pre-Hello deadline or an explicit
   ledgered availability limitation. (runtime.rs:268, connection.rs:195)

P3
8. Phase relation not enforced in release (send_replace precedes a debug-only assert); no illegal
   edge reachable today, but a future W3b2 call would publish unconditionally. (lifecycle.rs:121)
9. Test gaps: the reserved-drain test ACTIVELY READS the large frame so it cannot expose
   starvation/detachment; no coverage for replacement between identity-check and unlink, second
   signal during flush, over-limit drain reason, malformed/duplicate Hello, capability
   downscoping, raw-rejection EAGAIN/EPIPE. (Raw rejection itself verified sound: small alloc,
   no await, retries partial writes/EINTR, best-effort close.)

VERDICT: NO_SHIP — P1 1-3 break the drain barrier and exact endpoint-ownership guarantees this
lifecycle foundation must provide before downstream daemon lanes attach.
