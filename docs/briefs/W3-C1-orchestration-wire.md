# W3c1 — durable turn orchestration + wire (the keystone's engine room)

Lane branch: `w3-c` (worktree `~/haider-run/haider-w3c`, base 7395b6e).
Goal: a daemon that can CREATE sessions, RUN real turns (fake provider in tests), park on `request_input` via the menu CAS, cancel, and recover across restart — all over the real UDS wire. After this chunk, W3c2 (auto-spawn + login) and W3c3 (TUI live swap) are thin clients of it.

## Binding law (in precedence order)

1. `docs/research/w3c-research-report.md` — §3 (architecture, R1–R12 all DECIDED), §4 (orchestration flows), §5 (risk register), **§6.1 (this chunk's scope + its 13-scenario primary gate, verbatim)**. The report wins over this brief.
2. `docs/research/d1-daemon-research-report.md` §5.4–5.7 + R8–R14 — implemented law; W3c1 must not violate the six invariants (§5.5 INV-1/INV-2, R9, R12, R13, R14).
3. `docs/briefs/W3b2-review-4-SHIP_WITH_FIXES.md` — the hub-as-StoreHandle law and admission discipline W3c1 builds on. Workers NEVER touch SQLite directly; never await provider/tool work inside a session-hub actor arm.
4. `docs/OPTIMIZATIONS.md` — execute the W3c-triggered rows per R12; ledger new LATERs with triggers.

## Scope

Exactly report §6.1: the first mechanical commit (session_hub split into `session_hub/{mod,actor,replay,rpc}.rs` with NO behavior change; shared daemon/daemond UDS test support; `SessionHubConfig` through `DaemonConfig`), then the implementation list (wire-v1 method variants + `Welcome.features`; session metadata + durable command receipts; `session.create`/`turn.submit`/`turn.cancel`; external admission gate + worker-aware drain; `WorkerManager` per-session supervisor with owned joins; generation + active-worker-lease fencing; turn-scoped provider factory; prompt-history compiler + versioned system prompt; tool dispatcher + hub journal adapter + daemon CAS adapter; reasoning-safe Anthropic continuation + cumulative usage; durable `RunFailed`; retry owner; interrupted-run recovery; injectable production dependencies).

OUT of scope: auto-spawn, login, any TUI change, any live-API test as gate.

## Milestone commits (each independently green — commit at each)

- **M1** mechanical: hub split + shared UDS test support + config threading. Zero behavior change (the existing 530-test gate is the proof).
- **M2** wire + durability: method variants, `Welcome.features`, session metadata, command receipts, `session.create` end-to-end over UDS.
- **M3** the turn engine: `WorkerManager`, `turn.submit`/`turn.cancel`, provider factory, history compiler, tool dispatch, fencing.
- **M4** drain/recovery + the primary gate: worker-aware drain, restart recovery, and `haider-daemond/tests/live_turn_rpc_tests.rs` implementing ALL 13 numbered scenarios from report §6.1 verbatim (scenario 13 = the mutation-seam sweep).

## Discipline (standing law)

- Tests only go UP (baseline 530; `cargo run -p xtask -- test-count --update` after adding). Never delete or weaken a test; behavior-directed changes carry inline reasoning.
- MUTATION-CHECK LAW: every law-bearing test carries a `MUTATION CHECK:` comment naming the revert + expected failure; execute the load-bearing ones (report §6.1 scenario 13 makes this the gate's own shape) and record "Verified by revert" in commit messages.
- Full gate at every milestone: `cargo test --workspace`, `clippy --all-targets -- -D warnings`, `fmt --check`, `xtask test-count`. UDS suites need a socket-capable environment — if the sandbox denies binds, run the rest, say so precisely, and the orchestrator reruns.
- Additive wire only: the W3a golden transcripts must keep decoding byte-identically; new frames follow the Unknown-tolerant rules (skip-if-None, kind-tagged).
- Sandbox: if `git add` is refused on this linked worktree, leave the tree unstaged with a prepared commit message per milestone and report; the orchestrator commits.

## Review

Frozen-SHA dual review after M4 (gpt-5.6 correctness vs the 13-scenario gate + the six invariants; Fable design lens on WorkerManager shape and the hub seam). Verdict journals to `docs/briefs/W3c1-review-*.md`.
