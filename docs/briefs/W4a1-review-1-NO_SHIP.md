# W4a1 review of record — NO_SHIP (the consent boundary HOLDS; two P1 executor safety bugs)

Reviewer: gpt-5.6 (codex), frozen a1176d2, scope cc925b3..a1176d2 (W4a1).

**The headline is good: the approval boundary is AIRTIGHT.** P0: none found. 1a server-side dispatch gate HOLDS (full traced path: no fs mutation reaches the filesystem before a committed CAS approve; a second call can't overtake, cancel fences a late answer, restart-before-Dispatched resumes-or-loses never dispatches-without-grant); 1b durable session grants HOLD (only committed AllowAlways reconstructs, class from durable intent not client text, confined to SessionId); 1c CAS race HOLDS (deny-first beats late-approve, first-committed-wins, idempotent retries); 1d deny is typed no-fs, approve-once doesn't grant the next. Census reuse confirmed genuine (no parallel mutation impl). Invariants/seal/restart laws all HOLD. Mutation audit 4/4 killed (ask→allow sentinel killed by an exact policy guard — the real UDS sentinel couldn't start under the reviewer's AF_UNIX EPERM sandbox, but the guard proves the gate).

Required fixes (W4a1.1) — both reproduced with adversarial tests that pass on the frozen code:
1. **P1 — dynamic workspace escape (TOCTOU commit window).** filesystem.rs:913/957/1144. After the parent dirfd is opened and the temp created, rename the parent OUTSIDE the workspace + install an outside-pointing symlink at the original component; the held dirfd follows the moved dir, the leaf dev/inode check passes, renameat patches the now-outside file → SUCCESS. The committed swap test swaps BEFORE parent acquisition, covering only the O_NOFOLLOW walk. Fix: the confinement invariant must hold at RENAME time, not just at walk time — re-validate the parent still resolves under the canonical root immediately before renameat (macOS has no RESOLVE_BENEATH; re-canonicalize + verify the parent chain's identity is unchanged and under-root at commit, or hold the resolution against a root dirfd whose identity is rechecked). Pin the exact rename-time reproduction.
2. **P1 — same-inode concurrent edit silently clobbered.** filesystem.rs:913/973. After the patch temp is ready, externally truncate/write the target in place; dev+inode unchanged so the final check passes and the external content is overwritten → SUCCESS instead of typed PathChanged. Fix: capture the source's CONTENT identity at read time (hash, or size+mtime with the caveat noted) and re-verify at rename time, not just dev+inode; mismatch → typed PathChanged/conflict. Pin the in-place-edit reproduction.
3. **P2 — overlapping preimages misclassified as unique.** filesystem.rs:922. str::match_indices is non-overlapping: content "aaa" with preimage "aa" reports one match though 0 and 1 both start valid matches, and the first is silently patched. Fix: overlap-aware uniqueness (a preimage matching at overlapping offsets is ambiguous → typed conflict, not a silent first-match patch). Pin "aaa"/"aa".
4. **P3 — unpinned approval-commit→pre-dispatch crash window.** live_turn_rpc_tests.rs:3916. Add the pin: commit the answer, pause before the fresh Dispatched, crash, assert the ruled once-or-lost outcome.

W4a2 readiness noted: the actor/CAS/recovery bridge is reusable for ProcessExec, but the grant representation (Vec<EffectClass>, class-wide AllowAlways) is NOT shell-safe — W4a2 must add a durable per-command-shape scope key.

Release cannot merge. The approval/CAS boundary itself held under review, but the filesystem executor has two reproducible P1 safety failures: a dynamic workspace escape and a silent concurrent-edit clobber.

## Approval-boundary attack report

### 1a — Server-side dispatch gate: HOLDS

Exact path:

1. The model receives `fs_write`/`fs_patch` definitions from [worker.rs:2123](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2123).
2. Provider completion enters the actor-owned dispatcher at [actor.rs:1375](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1375).
3. Production policy sets `FsWrite => Ask` server-side at [worker.rs:2150](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2150).
4. Both mutation tools call `EffectBroker::begin` before spawning filesystem work: [filesystem.rs:401](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:401), [filesystem.rs:471](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:471).
5. `begin` journals `Dispatched` only after `Allow`; `Ask` returns typed `AuthorizationRequired`: [broker.rs:1043](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1043). The Ask verdict is persisted before the menu is registered at [broker.rs:856](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:856).
6. Daemon dispatch maps this to `ToolDispatchResult::ApprovalRequired` at [worker.rs:2282](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2282).
7. The actor persists `MenuOpened` and `InputRequired`, then waits only for a committed CAS envelope at [actor.rs:1448](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1448). Raw in-process answers are explicitly rejected at [actor.rs:1480](/Users/rizzist/haider-run/haider-agent/crates/haider-core/src/actor.rs:1480).
8. RPC approval requires control capability and a control attachment at [rpc.rs:1042](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/rpc.rs:1042).
9. The session actor calls the durable CAS, publishes the committed answer, then wakes the harness at [session_hub/actor.rs:332](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/session_hub/actor.rs:332).
10. Only then is the decision installed and the loop performs a fresh broker authorization and `Dispatched` append before scheduling filesystem work.

A second tool call cannot overtake the awaited call; concurrent submits are deferred. Cancel-before-answer closes the menu and fences a late answer. Cancel-after-committed-approval may still prevent dispatch, but cannot create an uncommitted dispatch. Restart before `Dispatched` either resumes the committed checkpoint or loses the operation; restart after `Dispatched` reconciles to `Unknown`.

No approval bypass was found.

### 1b — Durable session grants: HOLDS

Grant reconstruction scans only the session-bound store history and links durable `Intent → Authorized::Ask → Permission MenuOpened → MenuAnswered` at [worker.rs:2353](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2353).

Only a committed `AllowAlways` reconstructs a grant. Deny and approve-once do not. The class comes from the durable effect intent, not client text. The resulting grant is exactly `EffectClass::FsWrite`, intentionally shared by `fs_write` and `fs_patch`, and remains confined to that `SessionId`. A fresh session has an independent journal; no factory-global grant exists.

### 1c — Approval/request-input CAS race: HOLDS

Both use `Store::resolve_menu`, whose `IMMEDIATE` SQLite transaction implements first-committed-wins at [event_store.rs:1117](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:1117) and [event_store.rs:1774](/Users/rizzist/haider-run/haider-agent/crates/haider-store/src/event_store.rs:1774).

- Deny first: late approve receives `AlreadyResolved`; no grant is installed.
- Approve first: late deny cannot replace it.
- Approve-for-session versus deny follows the same ordering.
- Same-command retries are idempotent.
- Option key/index mismatch is rejected against the committed menu version.

### 1d — Deny and approve-once: HOLDS

Deny installs an exact class+digest one-shot denial at [broker.rs:969](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:969). The retry stops at authorization and returns a typed `permission_denied` tool result through [worker.rs:2424](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2424); no filesystem worker starts.

Approve-once is consumed by one fresh authorization at [broker.rs:1231](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/broker.rs:1231). The next identical mutation asks again.

## Findings

- **P0 — none found.** No path from a model `fs_write`/`fs_patch` call to filesystem dispatch without a committed approval or previously committed class-scoped session grant.

1. **P1 — dynamic workspace escape after parent acquisition.**  
   [filesystem.rs:913](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:913), [filesystem.rs:957](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:957), [filesystem.rs:1144](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:1144).  
   Reproduction: after the worker opens the parent dirfd and creates its temporary file, rename that parent outside the workspace and install an outside-pointing symlink at the original component. The held dirfd follows the moved directory; the leaf dev/inode check passes; `renameat` patches the now-outside file and returns success. The committed swap test performs the swap before parent acquisition and therefore covers only the `O_NOFOLLOW` walk, not this commit-window attack.

2. **P1 — same-inode concurrent edit is silently clobbered.**  
   [filesystem.rs:913](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:913), [filesystem.rs:973](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:973).  
   Reproduction: after the patch temp is ready, externally truncate/write the target in place. Device and inode remain unchanged, so the final check succeeds and the external content is overwritten. Current behavior returns success instead of typed `PathChanged`/conflict.

3. **P2 — overlapping preimages are misclassified as unique.**  
   [filesystem.rs:922](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:922).  
   `str::match_indices` is non-overlapping. Content `aaa` with preimage `aa` is reported as one match although valid matches start at offsets 0 and 1, so the first is silently patched.

4. **P3 — exact approval-commit/pre-dispatch crash window is unpinned.**  
   [live_turn_rpc_tests.rs:3916](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/live_turn_rpc_tests.rs:3916).  
   Existing restart coverage crashes while approval remains pending. No test commits the answer, pauses before the fresh `Dispatched`, crashes, and asserts the ruled once-or-lost outcome.

## Census reuse confirmation

Confirmed genuine reuse from `cc925b3`: `EffectClass::FsWrite`, `FsPatch`, preimage/replacement application, broker lifecycle, canonical path checking, dirfd/`O_NOFOLLOW` walking, locked source reads, same-directory temporary files, atomic rename, effect journaling, and ledger attribution already existed.

W4a1 extends that implementation with model exposure, `FsWrite`, approval previews/bridge, durable grant reconstruction, unique-match checking, and the final leaf identity recheck. It does not introduce a parallel mutation implementation.

## Mutation-check audit

| Mutation | Result |
|---|---|
| Production `FsWrite` policy `ask → allow` | Killed by an exact temporary production-policy guard. The real UDS sentinel could not start because this sandbox rejected AF_UNIX bind with `EPERM`, before daemon Ready. |
| Remove canonical `require_under_root` barrier | Killed by `mutating_paths_reject_parent_and_absolute_workspace_escapes`. |
| Remove final leaf identity recheck | Killed by `external_leaf_replacement_before_patch_rename_is_typed_path_change`. |
| Disable dispatched-effect reconciliation | Killed by `startup_reconciliation_is_durable_and_idempotent`. |

The two additional adversarial path/conflict repro tests returned success on the frozen code, demonstrating findings P1-1 and P1-2. All temporary tests and mutations were removed.

Final integrity: HEAD remains `a1176d27f20c2e1d5f4a08673c49920d20e4e4ac`; porcelain, staged/unstaged diff, and stash are empty.

## Invariant, seal, and restart laws

| Law | Result |
|---|---|
| INV-1 persist-before-publish | HOLDS |
| INV-2 receiver registration + head capture atomic | HOLDS |
| R9 actor-owned envelope sequence cursor | HOLDS |
| R12 store-backed bounded lag buffer | HOLDS |
| R13 durable first-committed menu CAS | HOLDS |
| R14 centralized capability authorization | HOLDS |
| Worker tools reach store only through `HubStoreHandle` lease | HOLDS |
| Held `Dispatched` without `Outcome` → `Unknown`, no redispatch | HOLDS |
| Crash after approval commit but before dispatch | HOLDS: resumes once while durably `InputRequired`, otherwise safely loses the operation |
| Static absolute/`..`/symlink confinement | HOLDS |
| Dynamic confinement after parent acquisition | **VIOLATED** |
| Concurrent patch conflict/no-clobber law | **VIOLATED** |

The worker seal is visible at [worker.rs:87](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:87); broker journal/CAS adapters retain only that lease at [worker.rs:2567](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/worker.rs:2567). Startup still reconciles effects before turn recovery at [runtime.rs:257](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/runtime.rs:257).

## Gate result

- Test census: `872 → 880`; `xtask test-count` reports `880`, matching baseline.
- Test-history audit: no deleted or weakened tests. The apparent removals are manifest relocation/extension and typed-result mechanical updates.
- `cargo fmt --all -- --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `git diff --check`: pass.
- Filesystem integration: 24/24 pass.
- Effect-broker approval tests: 13/13 pass.
- Durable menu CAS tests: 6/6 pass.
- Focused startup and panic reconciliation pins: pass.
- Full workspace compiled successfully with no `could not compile`. Eight UDS-dependent targets failed before product assertions because every AF_UNIX bind returned `EPERM`; consequently the real-daemon/FakeProvider CAS patch test and full 880/880 execution could not be certified in this sandbox.
- `44a0e2b` is the merge-base/ancestor of the frozen head. Merge remains blocked by the reproduced P1 findings.

## W4a2 readiness

The actor/CAS/recovery bridge is reusable for `ProcessExec`; `ApprovalRequired`, committed-menu-only waking, checkpoint recovery, and effect bindings are generic.

The persistent grant representation is not shell-safe yet:

- Production asks only for `FsWrite`.
- Session grants are currently `Vec<EffectClass>` and `AllowAlways` is class-wide.
- Shell execution needs durable per-command-shape scope, not a class-wide `ProcessExec` grant.
- Permission text assumes class scope.
- Shell streaming, cancellation, and output handling require dispatcher-specific adaptation.

W4a2 can reuse the bridge without remodeling the actor or CAS, but must extend broker policy and durable reconstruction with an explicit command-shape scope key.

VERDICT: NO_SHIP
