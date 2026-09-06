# turnperf round 2 — shared facts for the 16 latency/CPU lenses (cite; do not re-derive)

OWNER ASK (2026-09-01): "figure out how we can FURTHER reduce the wall median and the total CPU/wall time,
both on the daemon side and the non-daemon side; come back with NOVEL results." Deliverable per lens =
a candidate table (no code). Round 1 (8 lenses, 12 levers) already ran; its five implemented levers
FAILED verification (below). Re-proposing them is worthless — every candidate must be tagged
NEW / EXTENDS #n / SAME-AS #n (rejected) against the round-1 list at the end of this file.

TREE: /Users/rizzist/haider-run/wt-965 = wave-969 @ 3703746 (v0.0.968 + the 969 work listed below).
Read-only lens. No builds (the sandbox forbids writes; cargo will fail). Static analysis with file:line
citations. You MAY run the INSTALLED v0.0.968 binaries (/usr/local/bin/haider, /usr/local/bin/haiderd)
for light measurement (N<=20) under a throwaway HOME with HAIDER_DISCOVERY_DISABLED=1, ONLY if
load(1m) < 6 (`sysctl -n vm.loadavg`; a release build may be running — then do NOT time anything).
Stop any daemon you spawn with `haider daemon stop` under that HOME. Never kill haiderd broadly.
Never read/print ~/.haider vault/credential files or .env contents.

MEASURED (mark your own numbers MEASURED / DERIVED / ESTIMATED):
- Conformance bench, ONE-SHOT accounting (fresh daemon spawned per case via HAIDER_RUN_DAEMON_IDLE_TTL_MS=0,
  `haider run … --timeout 10s`, local fake proxy, 21 cases, load 3.2-3.5, macOS arm64 16 GB):
    metric            967     968     969-working   rick(no daemon)
    wall median ms    144     124     143           56
    wall total ms     12649   12357   12698         16173
    CPU total ms      1092    1059    1229          1318
    peak RSS max MiB  57.8    53.3    51.2          50.6
  969-working ≈ 968 on wall/CPU: it contains NO turn-latency lever (all failed, see below).
  Per case ≈ client exec + daemon spawn (36 ms cold: fresh HOME → spawn + `status --json`) + first-turn
  init + the turn + teardown. rick is a daemonless cold binary (56 ms) — the one-shot median can only
  approach it by shrinking spawn/init/teardown or by not spawning at all.
- Warm steady-state harness (scripts/qa-gate/turn_wall_harness.py + turnperf_support.py, standalone
  fake proxy, 25 measured + 5 warmups, trace OFF, load ~3): single-request turn 56.7 ± 3.5 ms,
  tool-call turn (2 provider requests + one process_exec) 78.0 ± 3.9 ms. `haider --version` 4 ms
  (exec floor, 34 MB binary); warm `status --json` 5 ms; `sessions --json` 5-9 ms.
- Journal: SQLite WAL + synchronous=NORMAL (event_store.rs:176) — commits are NOT device syncs. The
  one per-request device barrier is the provider-view CAS (F_BARRIERFSYNC, ~4 ms). Round-1 correction:
  the warm cost is TRANSACTION COUNT, SERIAL PUBLICATION BEFORE ACK, ROUND TRIPS and ADMISSION WORK.
- Memory (do not regress): daemon settled idle 5.4 MB, +191 KB/turn retained; client wire-only floors
  status 2.4 MB / run 3.0 MB; peak RSS max 51.2 MiB. Any candidate that raises these must say so.

ALREADY IN wave-969 (do not propose again; you may EXTEND with evidence):
- memdaemon 1+2: daemon Tokio workers capped at 4; SQLite release for spawned daemons; lazy OAuth/TLS
  shared HTTP client; bounded projections; per-turn retention instrumentation + cap.
- memclient 1: client cache shrink + CI footprint budgets (image buffer still eager — pending memclient2).
- contract: broad idle-TTL retirement, SIGINT → one durable turn.cancel → exit 130, kill-9 mid-turn →
  retry-pending on recovery, caller deadline bounds response-open.
- turnperf12: the warm harness, HAIDER_DAEMON_TRACE=1 stage trace port (client+daemon), SIGKILL
  boundary-sweep matrix (47/47, 0 duplicate provider requests). Trace-on breakdown is being run now.
- upstretry state-based retry classification; toolargs; oauth test determinism.

ROUND-1 LEVERS AND WHAT HAPPENED (turnperf/PROPOSAL.md in the job tmp if you need the full text):
 #1 fold Streaming+first fact / Usage+Completed+NodeCommitted+Done into fewer transactions → implemented,
    measured NULL (no wall change) — transaction count was not the cost it was assumed to be.
 #2 one atomic headless.run RPC (create+accept+attach) → implemented, REGRESSED 2× (58→112, 78→159 ms),
    reverted. Think about WHY before proposing any RPC fusion: it serialized work that was overlapping.
 #3 complete commands before fan-out / client-first publication → NOT RUN (same family as #2, doubtful).
 #4 dedicated ordered SQLite store thread instead of spawn_blocking → NOT RUN.
 #5 admission fusion (lockdown bind+activate one write, quota off send path, cached project-instruction
    snapshot) → −10 ms but UNSAFE: matrix 48/51, duplicate provider request at the fused boundary; CPU up.
    A variant that caches admission WITHOUT moving a durable boundary (#5b) is still open.
 #6 defer graph telemetry / selective digests / batched catch-up → not run.  #7 4 workers (LANDED),
    route watch + liveness deadlines (queued).  #8 client single event sink / lazy profile / exit on
    correlated terminal (queued).  #9 SQLite pragma pins + statement cache + maintenance checkpoint (queued).
 #10 borrowed SSE decode / move-only usage (queued).  #11 tool-boundary transaction fusion (not run).
 #12 proof infra (LANDED).
Lesson: estimates from static reading were 2-4× optimistic and two levers made things worse. Prefer
candidates whose cost you can point at (a sleep, a poll interval, a redundant round trip, a repeated
parse, a syscall storm, an eager init) over ones that re-architect commit boundaries.

OUTPUT (one table, then ≤8 lines of ordering/measurement plan, then LAST line SHIP or NO_SHIP):
id | side (daemon/client/both) | candidate | mechanism + file:line evidence | what it costs today
(MEASURED/DERIVED/ESTIMATED ms, and which metric: one-shot wall median / warm turn / CPU total) |
expected saving | durability & contract impact (docs/jsonl-run-contract-v1.md, automation-contract-v1.md,
client-contract-v1.md) | memory impact | risk | effort S/M/L | confidence | novelty (NEW / EXTENDS #n / SAME-AS #n)
Rank by expected saving × confidence. Fewer, better-evidenced rows beat long lists.
