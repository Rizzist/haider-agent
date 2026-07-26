# W3a review round 1 — NO_SHIP

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: 442382a (scope 8dcfd0c..442382a: lane 44d16c4 + clean-code 56e0f35 + rider 442382a)
- R-compliance: R6/R7/R14/R18/R19 COMPLY; R9 VIOLATES (P1-1).
- Full log: ~/haider-run/w3a-review-r1.log

## Findings (fix in W3a.1)

P1
1. frame.rs:361 Lagged.resume_after_seq makes a DAEMON-QUEUED position the resume
   authority; R9 requires the client's greatest fully APPLIED seq (queued != applied —
   a slow client can skip durable events). Rename to last_queued_seq (informational),
   document that clients resume from their own last_applied.
2. uds_codec.rs:87 completed-frame delivery depends on OS chunking: valid frame + bad
   prefix in ONE push() discards the decoded frames vec; same bytes split at the
   boundary delivers then errors. Fix: return frames decoded so far WITH the error
   (or buffer-then-error deterministically) — streaming transcript invariance.

P2
3. frame.rs:97 handshake omits client_name/client_version/client_instance_id/
   max_receive_frame/profile_id/daemon_version — W3b cannot honor the client's
   receive ceiling.
4. frame.rs:214 SessionList numeric page/page_size vs report §5.4 cursor tied to a
   stable ordering key — concurrent mutation dups/skips; wire-breaking to change later.
5. frame.rs:339 Event lacks attachment_id — multi-attach/catch-up association ambiguous.
6. frame.rs:248 no correlated error response (ResponseBody success-only; ProtocolError
   uncorrelated) — CursorAhead/CapabilityDenied/AlreadyResolved need request_id.
7. frame.rs:353 MenuAnswer one bare string; needs stable key/index + optional
   free-form/secret-ref input to bridge the domain Menu type.
8. frame.rs:368 ServerDraining missing reason/instance/generation; deadline_ms
   duration-vs-timestamp ambiguity must be pinned.
9. frame.rs:317 report §5.2 wants top-level Ping/Pong (report wins over brief).
10. codec.rs:111 rider growth cap not real: try_reserve amortizes past the target
    (probe: 104-byte limit → capacity 128). Geometric target + try_reserve_exact(additional).

P3
11. Golden fixture pins pretty-printed serde output, not compact WS bytes / UDS-prefixed
    bytes — production byte changes could pass goldens. Pin the REAL wire bytes.

VERDICT: NO_SHIP — R9 violation + chunking-dependent UDS delivery; v1 shapes W3b
cannot build on.
