# W3c3 — TUI live swap (THE v0.0.12 KEYSTONE)

Lane branch: `w3-c3` (worktree `~/haider-run/haider-tui2`, base c74c409 = full daemon stack + client merged).
Goal: bare `haider` (and `haider tui`) enters the LIVE TUI on the real daemon — create/attach/submit real sessions, answer real menus, `/login anthropic api` from the masked card — while `haider tui --demo` remains byte-deterministic. This release is the product's demo→real moment.

## Binding law (precedence order)

1. `docs/research/w3c-research-report.md` **§6.3 (scope + tests, verbatim) + §6.4 (the FINAL ACCEPTANCE MATRIX — the release gate)** + R11 (DECIDED migration order: identity → raw-envelope/chip reduction → demo vocabulary isolation → LiveDriver; --demo retained). Report wins.
2. The Fable W3C SEAM REPORT in `docs/briefs/TUI4-review-1-NO_SHIP.md` — the authoritative map of what dies with DemoDriver, the 3 cuts (PurgeDemoStore vocabulary, absorb(DemoEvent) chip variants, u64 ids vs SessionId), and the verified stays-put list (reducer, projections, status derivation, animated(), render, select/clipboard, hit map, input pump — these MUST NOT be remodeled).
3. `docs/briefs/W3c2-review-1-SHIP_WITH_FIXES.md` — the W3c3-readiness section (RpcClient surface, login-card mapping, reconnect primitives) + the two carried pins this lane owns.
4. The TUI corpus: the sim (read-only law for demo behavior), docs/briefs/TUI5-review-1-SHIP_WITH_FIXES.md (7 carried P3s — fold F4 mouse-side gate if trivially adjacent), the probe ladder discipline.

## Scope (report §6.3 verbatim)

`SessionId` migration + separate `UiGeneration`; raw-envelope session router with STRICT gap behavior (gap → stop + reattach request before later state mutates); capped active/running/pending-menu attachment working set (16, LRU-detach) with cold list/read; agent-event chip projection; live response/action model; `LiveDriver` reconnect/reattach + command outbox; live launcher create/attach/submit order (no row/session until daemon responses/events arrive); live menu coordinates (exact opening sequence/generation + same-command retry); `/login` argument slots + masked card + stage/login result handling (typed restage_required/busy recovery text); demo-only DemoEvent/persistence/reset/arm/meter/answer-echo remain under `run_demo`; demo-store v1 numeric-ID upcaster + v2 string-ID rewrite; bare `haider` and `haider tui` enter live mode; `haider tui --demo` stays deterministic.

PLUS the two carried pins (W3c2 review): the pending-command secret TTL paused-time pin (retryable login, advance past SECRET_TTL, assert restage_required + wipe) and the five stable-code literal pins (unauthorized/permission_denied/restage_required/vault_unsupported/credential_missing as wire strings, extending the W3c1 literal-pin precedent) — land BEFORE the login card matches on those codes.

## Tests (report §6.3's list is the gate — all of it)

Reducer duplicate no-op; gap stops + emits reattach before later state mutates; reconnect restores the bounded priority working set after last applied cursors, LRU-detaches before the 17th attach, cold sessions listable/readable; background-session envelopes route by opaque id; agent spawn/state/report populate nested chips; attach response precedes first event, unknown attachment ids rejected; launcher creates no row/session until daemon truth arrives; menu answer carries exact opening sequence/generation + same-command retry; secret typing/paste/redraw/copy/error/quit/panic-teardown never reveal the key (the TUI leg of the sentinel sweep — the leg W3c2 documented as this lane's); all existing demo snapshots/goldens pass under --demo; persisted v1 numeric demo fixture upcasts without reseeding and rewrites as v2; render benchmark p95 on 1k/3k/5k replays w/ the ledgered cache only at its threshold.

## The acceptance matrix (§6.4) is the v0.0.12 release gate

Every row, verified over real PTY where user-visible: clean profile `haider` → one detached haiderd + live TUI; second terminal attaches, sees contiguous live events; FakeProvider path deterministic with no network (a live-mode PTY probe: real daemon + injected FakeProvider driven end-to-end — new scripts/tui-probes/pty-probe-live.py on probelib); menu answers from either control attachment resume exactly once; cancellation reaches one terminal; restart never repeats ambiguous work; lost mutation response duplicates nothing; version mismatch explicit + non-destructive; --demo regression-pinned; full gate. The real-Anthropic row is owner-run evidence, never the merge gate.

## Milestones (each independently green + full DEMO probe ladder still passing)

- **M1 identity + router**: SessionId/UiGeneration migration, raw-envelope router + strict gaps, chip projection from envelopes, demo vocabulary isolation (the 3 cuts), demo upcaster. Demo mode entirely green (snapshots/goldens/ladder) — this milestone changes plumbing, not behavior.
- **M2 LiveDriver + live surfaces**: reconnect/reattach/outbox, working set, live launcher/session/menu flows, bare `haider` + `haider tui` live entry.
- **M3 login card + acceptance**: /login slots + masked card + TTL/code pins, the TUI sentinel leg, pty-probe-live, the full §6.4 matrix, render benchmark.

## Discipline (standing law)

Tests only UP from 724 (xtask test-count --update per milestone). Never delete/weaken; directed changes carry inline reasoning (demo tests being RE-SCOPED to --demo paths is expected — flag each). MUTATION CHECK on law-bearing tests; execute load-bearing reverts ("Verified by revert"). The stays-put list is law — a live swap that remodels the reducer/render has failed R11. Full gate + FULL probe ladder (demo 14 runs + live probe) per milestone. The sim stays read-only. Secrets never in snapshots/logs/state files. Review after M3: frozen-SHA review of record (Fable) + the §6.4 matrix walk gates the merge → v0.0.12 tag → release → install.
