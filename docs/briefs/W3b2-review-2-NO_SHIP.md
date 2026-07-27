# W3b2 review round 2 — NO_SHIP

- Frozen SHA: 584d7de (scope e2aee54..584d7de). Full log: ~/haider-run/w3b2-review-r2.log
- Closure: 17 of 21 round-1 items CLOSED (incl. both P1 pacing bugs — atomic byte-aware offer
  verified, five-lane + large-envelope tests pass, no snapshot path remains). PARTIAL: P1-3
  (drain × pending-internal-lag), P2-5 (Lagged can purge the staged attach response), P2-7 +
  P3 (the uncancellable-store exception is NOT read-only — append/CAS use the same adapter),
  D2-11 (2 GiB aggregate not a hard bound — empty-channel exception exceeds it).
- ADJUDICATION (a) atomic offer: ACCEPTED for atomicity (encode-then-check under one mutex,
  grant≡consume, lost-wakeup closed by subscribe-before-offer + credit signaling). NOT yet
  sufficient for "reading clients never lag": Busy waiters race a broadcast with no FIFO, so
  hot lanes can starve a cold lane into catch-up lag.
- ADJUDICATION (b) class-split admission: REJECTED in current form. The A+1-lane premise is
  false — lag_and_detach releases the slot, purges the lane, then RECREATES it ownerless for
  Lagged; repetition accumulates ~154k detached lanes per default connection (109-byte Lagged
  frames against a 16 MiB budget), map/key/deque overhead uncharged. Also: UDS +4 prefix bytes
  make the true ordinary byte bound max(B, L+4); the ¼ reply headroom doesn't protect a legal
  large SessionRead reply; W3b1's aggregate-depth guarantee for every ordinary frame was
  replaced by a false bound (explicitly, but false).

## New findings (fix in W3b2.4 — SIMPLIFY, do not add another epicycle)
P1
1. Drain × catch-up overflow: a commit overflows catch-up setting real lag; shutdown stops the
   actor before replay observes it; closed-watch handling suppresses the pending lag and the
   committed suffix is never store-resumed nor broadcast.
2. Forced-stop fence raised AFTER actor aborts (declaration-order drop): an actor on another
   worker can receive a queued append/CAS, see the fence false, and start an uncancellable
   blocking write after deadline/second-signal.
3. Detached Lagged lanes (the class-split rejection above).
P2
4. Lagged can precede or REPLACE the purged staged attach response → client sees an unknown
   attachment id.
5. Busy admission has no starvation discipline (broadcast race; no FIFO).
6. ¼ reply headroom insufficient for legal large replies.
7. Valid configs exceed the documented byte ceiling (B = frame_limit + 4-byte UDS prefix +
   empty-queue admission).
8. Catch-up aggregate not a hard worst case (empty-channel oversized admission).
P3
9. Uncancellable-store exception documented as read-only; append/CAS share it and may commit
   without publication on the FORCED path.

VERDICT: NO_SHIP.
