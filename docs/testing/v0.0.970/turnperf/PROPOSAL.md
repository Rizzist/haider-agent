# turnperf PROPOSAL (synthesis of 8 lenses, 2026-09-01) — targets: warm single-request <= 40 ms, tool-call <= 60 ms, durability retained

CORRECTION TO THE HYPOTHESIS: the journal is WAL + synchronous=NORMAL (event_store.rs:176) — ordinary
commits are NOT full-device syncs. The only per-request device barrier is the provider-view CAS
(one F_BARRIERFSYNC per physical request). The warm turn cost is TRANSACTION COUNT, SERIAL
PUBLICATION BEFORE ACK, ROUND TRIPS, and ADMISSION WORK — not fsyncs.

| # | lever | stage | est. saved (single / tool) | durability | effort | conf | lenses |
|---|---|---|---:|---|---|---|---|
| 1 | Fast-ready response transaction: fold Streaming + first stream fact, and Usage+Completed+NodeCommitted+Done into one guard-aware commit | provider open -> terminal | ~20 / 28-40 | same facts, fewer transactions; crash before commit = clean retry (attempt marker) | M | high | L1 L2 L5 L8 |
| 2 | One atomic headless.run RPC (create + accept + attach; response carries session/run/attachment/high-water) | client <-> daemon | 3-6 / 3-6 | one acceptance transaction instead of two; receipts unchanged | M | high | L3 L4 L7 L8 |
| 3 | Complete actor commands after commit but BEFORE fan-out; client-first publication (committed envelope straight to the attached JSONL stream; projections/hooks/telemetry on an ordered sidecar lane) | commit -> client | 5-10 / 5-10 | persist-before-publish kept; only observers move after | M | high | L4 L6 L8 |
| 4 | Dedicated ordered SQLite store thread (bounded queue, oneshot completion) replacing per-call spawn_blocking; absorb the append committer | every store call | 3-8 / 5-10 | identical transactions; crash atomicity unchanged | M | high | L2 L7 L8 |
| 5 | Admission path: fuse lockdown bind+activate into one ledger write; move quota enumeration off the send path; cache validated project-instruction snapshot per session (today walks up to 256 ancestors per prompt); eager session supervisor | accept -> request | 5-15 / 5-15 | lockdown boundary committed with the attempt marker | M | med-high | L4 |
| 6 | Defer graph-telemetry persistence to terminal delivery; selective, incrementally-sized ObserveDigest; batch attachment catch-up per committed batch; range-compress hook outbox rows | post-commit fan-out | 3-8 / 5-12 | rebuildable projections only; journal untouched | M | med | L6 |
| 7 | Runtime: cap daemon workers at 4 (bench 4 vs 6); replace the 250 ms route poll with a reachability watch/Notify; resettable one-shot liveness deadlines | scheduling | 1-4 / 1-4 | none | S | med | L7 L3 |
| 8 | Client: single event sink for JSONL (drop channel hops + blocking adapter thread), lazy profile/config materialization before connect, exit on correlated terminal | client | 2-5 / 2-5 | none | S | high | L3 |
| 9 | SQLite hygiene: pin fullfsync=OFF / checkpoint_fullfsync=OFF explicitly, cache remaining hot statements, checkpoint via a maintenance connection at the 1000-frame watermark | store | 1-3 / 1-3 | none (already NORMAL) | S | high | L2 |
| 10 | Streaming micro-wins: Bytes/borrowed SSE decode, single-materialization completion, move-only usage; compact attempt-marker rows | provider parse | 1-3 / 2-5 | none | S | med | L5 |
| 11 | Tool boundary: fuse durable tool result + restored Streaming + provider-view transition + next attempt marker into one transaction | tool-call path | 0 / 8-12 | one ordered transaction; crash = retry from marker | M | med | L8 L5 |
| 12 | PROOF (prerequisite): steady-state harness (warm daemon + standalone fake_proxy, 25 measured + 5 warmups per shape, MAD), trace-only port of haider.turn points from lane-967-pipeline (b2351c7 not cherry-pickable as a unit), SIGKILL boundary-sweep matrix (kill after every transaction -> replay parity, one terminal, no provider double-issue), CI turn-wall budget | measurement | enables all | this is the durability guarantee | M | high | L8 |

Non-additive; realistic warm single-request after 1-5: ~30-40 ms (from ~70); tool-call ~45-60 ms.
Order: 12 (harness+trace) -> 1 -> 2 -> 3 -> 4 -> 5 -> 6 -> 7/8/9/10 -> 11. Hold-out rule per lever; CPU and peak-RSS constraints as memory lanes.
