# TUI6 review round 4 — NO_SHIP (the 6.3b in-wave push regressed a P1; P2 paste hygiene closed)

Reviewer: gpt-5.6 (codex), frozen 65d9353, scope fd22d66..65d9353 (TUI6.3 + TUI6.3b).

**ORCHESTRATOR NOTE (own the mistake):** I directed TUI6.3b as a "small closing commit" to land the staged-tag liveness P3 in-wave. That was wrong. The r3 P1 fix (attempt identity) was sound, but 6.3b's FIFO-position no-ID consumption turned a fail-closed wedge into a real credential-binding hole. The lesson: a fix that correlates by QUEUE POSITION on a wire that sends UNCORRELATED (no-id) frames is structurally unsound the moment the queue mixes a retired and a live attempt. Do not paper a liveness wedge with position-based popping — carry identity.

Closed: P2 paste hygiene (Debug redacted; Pasted::new preserves pointer+capacity; Zeroizing at receipt, wiped on drop; thresholds byte-identical; paste-behind-menu still dropped). Normal P1 ordering passes (old Staged mints nothing, old LoggedIn can't touch the new card, 1001 monotonic ids, all abandon paths clear before early-return). Mutation audit 4/4 killed+restored. 861→866→868, no deletions.

Required fixes (TUI6.4 — the real close):
1. **P1 — cross-attempt binding via no-ID FIFO pop.** After abort→re-login the stage queue is [retired N, live N+1]; a NON-stage Failed{command_id:None} (List/Detach errors link.rs:625, uncorrelated ProtocolError link.rs:687 — response waiters spawn independently, protocol allows out-of-order correlated replies) consumes N at live.rs:1002; the OLD attempt's Staged then consumes N+1, passes both live gates, and mints LoginApi with the OLD-CANCELLED-VAULT-REFERENCE at live.rs:805 (probe-observed). Structural fix: stage-reply correlation must be by ATTEMPT IDENTITY, not queue position — the Staged/stage-error reply must carry (or be matched to) the attempt id the way the r3 fix already threads it through LoginApi; a reply whose identity is retired or not the live attempt is dropped. This likely lets 6.3b's no-ID positional consumption be REMOVED entirely (the liveness P3 it targeted is then solved by identity-matched stage replies + the existing 30s deadline, not by popping). If a residual no-id-error liveness case remains, it must be genuinely fail-closed (never mint on ambiguity) and deadline-bounded — re-argue it honestly.
2. **P2 — retired Failed paints a misleading global flash** (live.rs:1002 area): a late Failed(Some(old_login_id)) for a retired attempt leaves the card untouched but sets model.flash = "· provider_rejected — old attempt failed". Retired replies must be SILENTLY ignored (no flash) — the user already saw the cancel.

Vault-reference note: whether the daemon would honor a stale single-use vault reference is a daemon-side backstop; the TUI must NOT emit it regardless. Verify the TUI-side fix independently of any daemon idempotency.

P1 and 6.3b do not close. P2 paste hygiene closes. Merge and v0.0.13 remain blocked.

### Closure rulings

| Area | Ruling | Evidence |
|---|---|---|
| P1 attempt identity | **Not closed** | Normal abort→re-login ordering passes: old `Staged` mints nothing and old `LoggedIn` cannot touch the new card. Queued `LoginApi` retirement, same-pass `[LoginRetired{N}, LoginApi{N+1}]`, reconnect-login, and 1,001 strictly monotonic IDs also pass. However, the 6.3b ambiguity enables cross-attempt credential binding. |
| P1 retired `Failed` | **Not closed** | A late `Failed(Some(old_login_id))` leaves the card untouched but paints `model.flash = "· provider_rejected — old attempt failed"`. Retired replies are therefore not silently ignored and the required “no recovery paint” condition fails. |
| P2 paste hygiene | **Closed** | Debug is redacted; `Pasted::new` preserves pointer and capacity exactly; the buffer is moved into `Zeroizing<String>` at receipt and wiped on drop. The handler only borrows it before copying into the card’s zeroizing buffer. Composer thresholds and newline behavior are byte-identical; paste behind a blocking menu remains dropped. |
| 6.3b | **Not closed** | Single-live-tag stage errors and clear-before-early-return work, but the documented non-stage/no-ID residual is production-reachable and neither fail-closed nor deadline-bounded. |

### Blocking residual attack

After abort and immediate re-login, the stage queue can be `[retired N, live N+1]`.

1. A non-stage `Failed { command_id: None }` consumes `N` at [live.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1002).
2. The old attempt’s subsequent `Staged` consumes `N+1`, passes both live-attempt gates, and mints `LoginApi` using the old vault reference at [live.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:805).
3. The external probe observed:

`LoginApi { vault_reference: "OLD-CANCELLED-VAULT-REFERENCE", ... }`

Production sources of unrelated no-ID failures include `List`/`Detach` response errors at [link.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:625) and uncorrelated `ProtocolError` at [link.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:687). Response waiters are independently spawned, and the protocol supports out-of-order correlated replies.

This can commit a cancelled credential under the new card. The documented “strictly no worse than the wedge” claim is false.

All `abandon_login` paths were enumerated:

- `Disconnected`
- timeout via `expire_login`
- `Reconnected → resume`

All route through [abandon_login](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1111), where tags and attempt binding clear before its sole early return. Reconnect followed immediately by login passes.

### Mutation audit

| Revert mutation | Confirmed failure |
|---|---|
| Weaken `Staged` attempt gate | Old cancelled reference minted `LoginApi`; `cancelled_attempt_never_mints...` failed. |
| Disable queued-request removal on close | Cancelled `LoginApi` survived dispatch; `close_retires_the_queued_login_request...` failed. |
| Disable 6.3b no-ID consumption | Live stage error did not recover immediately; `stage_error_pops_its_tag...` failed. |
| Move abandon queue clearing after early return | Stale tag consumed the post-reconnect reply; `retire_then_disconnect...` failed. |

All four were restored. `app.rs` and `live.rs` hashes match `HEAD`.

### Regression and gate

- Test ledger: **861 → 866 → 868**; `xtask test-count`: **868/868**.
- Test delta: seven tests added, none deleted. All nine removed test lines were mechanical `Paste(String)`→`Paste(Pasted)` plumbing; no assertion was removed or weakened.
- Login suite: 19/19. TUI6 suite: 49/49.
- Release CLI/daemon build: pass.
- Clippy `--all-targets -D warnings`: pass.
- Formatting: pass.
- Workspace command: no compile failures; eight UDS/process-backed targets failed with 82 explicit sandbox `PermissionDenied`/`Operation not permitted` lines.
- Ladder: 14/14 demo rows passed. Both live rows failed before alt-screen/daemon startup under the same UDS denial; supplied orchestrator evidence remains 16/16.
- Synthetic merge against `d9d66b4`: no conflict markers; `git diff --check` passes.
- Final state: frozen `65d9353ffbf01e85a190c5c2ca90a5c2aa5e9a23`; porcelain, tracked/index diff, and stash list empty.

### Law table

| Law | Ruling |
|---|---|
| Login modality — secret hygiene + correlation + liveness | **Violated:** hygiene passes; correlation/liveness fail through the cross-attempt residual and retired-failure paint. |
| Everything-else regression | **Unchanged:** directed TUI and demo regressions remain green. |

### New findings

| Tier | Finding |
|---|---|
| P0 | None found. |
| P1 | Non-stage no-ID failure can shift `[retired, live]` stage correlation and let the old `Staged` reply mint a login using the cancelled vault reference under the new attempt. |
| P2 | A retired login command’s late `Failed` reply paints a misleading global recovery flash instead of being ignored. |
| P3 | None found. |

VERDICT: NO_SHIP
