# turnperf — shared facts for the per-turn latency lenses (cite; do not re-derive)

OWNER TARGET (2026-09-01): warm single-request `haider run` turn <= 40 ms wall, tool-call
turn (2 model requests) <= 60 ms, WITHOUT giving up durability. Deliverable = a proposal
table (no code): every lever that shortens the turn while keeping the durability contract.

MEASURED on installed v0.0.968 (main d75a8ea), quiet machine (load ~4), macOS arm64:
- `haider --version` 4 ms (exec floor, 34 MB binary). Warm daemon RPC: `status --json`
  5 ms, `sessions --json` 5-9 ms. Cold: fresh HOME -> spawn haiderd + status = 36 ms.
- Conformance bench (one daemon per case, cold): single-model-request `run` case 96-120 ms;
  each extra model request +15-35 ms (tool-call cases 132-173 ms); retry_500 1.1 s (one
  backoff sleep); timeout case 15 s (policy). Warm ESTIMATE per single-request case
  55-78 ms (= cold - 31 spawn - ~10 cold caches). rick (no daemon, no journal): 54 ms.
- So the warm turn pipeline is ~55-75 ms. Hypothesised split: client exec+connect+hello
  ~6; durable boundaries ~25-35 (F_FULLFSYNC ~4 ms each on this Mac, measured in 967;
  968 resume added a per-attempt durable marker => ~6-8 boundaries per turn); provider
  round trip to a local fake proxy ~5-8; event fan-out/observe/hooks/JSONL ~10-15; Tokio
  hops (every SQLite call is spawn_blocking; 10 workers) ~5-10. VERIFY these from source.
- Prior wins (do not re-propose as new): 967 group commit + transaction batching;
  login_receipts -87.7% with F_FULLFSYNC boundaries 96 -> 0; provider-view CAS moved to
  the barrier tier (remainder 4.5 ms once per new profile); FIX0 -10 ms/request;
  968 hook engine no longer decodes every event when no hooks are installed.
- Long-lived daemon facts: LINGER default 30 s idle TTL (HAIDER_RUN_DAEMON_IDLE_TTL_MS);
  969 lane `warmdef` makes warm-by-default + prewarm. Assume the daemon is WARM.

DURABILITY CONTRACT (must survive every lever; cite docs/jsonl-run-contract-v1.md,
docs/automation-contract-v1.md, docs/client-contract-v1.md, docs/event-schema-changelog.md):
- The journal is the source of truth; `RawEnvelope.seq` is the sole cursor; replay of a
  run must reproduce the same events and exactly ONE typed terminal (run_state done/
  errored/cancelled + adjacent run_failed on failure) — replay parity is tested.
- kill -9 at any point must leave a recoverable, non-corrupt store; 968 recovery re-enters
  retry for a turn interrupted before its response; request-attempt markers exist so a
  restart does not double-issue a provider request. Say for each lever WHICH durable
  point it moves/merges and WHY the guarantee still holds (what is lost on crash and
  whether that loss is contract-visible).
- SQLite WAL is in use; autocheckpoint at 1000 frames; F_FULLFSYNC on macOS is the real
  cost (~4 ms), plain fsync is ~0.3 ms but does not flush the disk cache.

RULES: READ-ONLY lens. No builds, no product edits. Static analysis of
/Users/rizzist/haider-run/wt-965 (main = v0.0.968) with file:line citations; you MAY run
the INSTALLED binaries (/usr/local/bin/haider, haiderd) for light measurements (N<=20,
report load, refuse to time if load(1m) > 6) under a throwaway HOME; stop any daemon you
spawn with `haider daemon stop` under that HOME — never kill haiderd broadly. Mark every
number MEASURED / DERIVED / ESTIMATED. Output ONE table:
lever | stage of the turn | expected ms saved (single-request / tool-call) | durability
impact (what moves, why still safe) | CPU/peak-RSS impact | risk | effort (S/M/L) |
confidence. Then <=6 lines: the ordering you would implement and the harness/pins that
prove each lever kept durability. LAST line: SHIP (table with evidence) or NO_SHIP.
