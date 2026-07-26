# W3a review round 2 — SHIP_WITH_FIXES (fixes completed)

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: 6a5634e (scope 442382a..6a5634e)
- Closure: 10 of 11 r1 findings CLOSED (both P1s proven via independent probes —
  chunk invariance at every split point incl. valid+valid+bad-in-one-push,
  poison-then-more, error-at-offset-0; encoder capacity probed over 726+480
  exact/near-limit cases incl. ~8MiB, never exceeding max(limit, encoded_len)).
  R-compliance: ALL of R6/R7/R9/R14/R18/R19 COMPLY (R9 violation resolved).
- One PARTIAL → the enumerated required fix: ResponseBody::Error carried no
  structured recovery data for cursor_ahead / already_resolved (report §5.4/§5.6).
- Full log: ~/haider-run/w3a-review-r2.log

## Required fix — COMPLETED post-verdict (this commit)
Added `ErrorData` (kind-tagged, #[non_exhaustive], Unknown-tolerant):
`CursorAhead { requested, head }` and `AlreadyResolved { resolution_seq }`;
optional `data` field on ResponseBody::Error (skip-if-None — existing error
frame bytes UNCHANGED, proven by the +8-line-only fixture delta); two typed
frames added to the conformance transcript (both codecs, real-byte goldens);
explicit tolerance test (absent data → None; unknown future kind → Unknown).
Baseline 338→339.

VERDICT: SHIP_WITH_FIXES — fixes completed; W3a is merge-ready.
