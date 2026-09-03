# wsroot — vanished workspace recovery (lane-970-wsroot)

Verdict: **SHIP**.

Tested on macOS on 2026-09-03 from `lane-970-wsroot` at `bd9aa61` plus the
uncommitted lane diff. The common brief named base `8952219`; this worktree was
already at the later owner-provided base `bd9aa61`, so the lane was not rebased.
The owner-supplied `LANE-COMMON.md`, `LANE-BRIEF-wsroot.md`, `turnperf/`, and
`turnperf2/` evidence remain untracked and unchanged.

## Root cause and correction

The fatal path was ordinary turn setup constructing `EffectBroker` in
`worker.rs`; `EffectBroker::new_at` canonicalized the stored root, and a deleted
root escaped as a generic `ToolError`. `worker::tool_error` then assigned the
generic `provider-error` class, leaving the run errored with no recovery action.
Project-instruction and hook discovery each had separate canonicalization paths
that also needed a root-availability gate.

The correction has four parts:

1. `WorkspaceUnavailableReason` classifies `missing`, `not_directory`, and
   `not_readable`. `workspace_unavailable` is a real non-provider error code and
   an additive typed event containing the stored path, reason, and bounded
   detail. It is emitted once per logical run with `ui=true`, `durable=true`,
   and `prompt=omit`; raw JSONL preserves the event exactly.
2. Attach and turn setup run a cheap `metadata` plus open-directory probe over
   the already-canonical stored root. No full canonicalization was added to the
   ordinary-turn hot path. Turn setup repeats that cheap probe immediately
   before broker construction, and a constructor-level typed fallback closes
   the remaining disappearance race. An unavailable root skips project
   instructions, child handoff discovery, hook discovery, and broker/receipt
   construction.
   Plain chat still reaches the provider with a volatile degraded-workspace
   note. Every broker-routed tool completes as a typed rejected tool result,
   never a provider failure or effect receipt; actor-owned conversational tools
   remain usable. The worker synchronously pins the unavailable run in the hook
   service before any await. Store schema v28 also records the run and its
   unavailable fence on hook-dispatch outbox rows, so restoring a directory at
   the same pathname cannot re-enable discovery or firing for that run.
   Pending workspace hook subscriptions are reconciled away and affected rows
   are acknowledged rather than firing after a later re-root.
3. `session.workspace.set` performs full fresh workspace validation only for a
   new mutation and retains the opened directory descriptor through actor/store
   commit, then atomically updates typed session metadata and commits a
   `workspace_selected` fact plus command receipt. Receipt lookup precedes path,
   generation, and filesystem validation, so response-loss replay remains exact
   even if the selected root vanishes later. A fresh mutation is serialized
   against turn admission and requires an idle session, so no old-root broker
   can outlive the boundary. The same transaction removes every pre-selection
   hook-outbox row and clears the durable unavailable-run fence; the selection
   fact carries `previous_path` so old-root live subscribers and servers are
   retired even when the new root has no hooks.
   Re-rooting also clears historical root-scoped instruction, permission,
   binding, and freshness reductions.
4. The TUI projects both workspace facts without treating them as unknown.
   `/retry` on an unavailable session offers `re-root to <current cwd>`, commits
   the workspace selection, updates the live generation/cwd, and only then
   retries the failed run. The headless CLI accepts the requested
   `haider session workspace set <path>` shorthand when the profile has exactly
   one unambiguous session, and retains the explicit multi-session form
   `haider session <session-id> workspace set <path>` (the RPC method is exactly
   `session.workspace.set`).

The project-instruction secure walk itself is unchanged. The cheap root probe
is strictly in front of that walk, so unavailable roots never enter it and this
lane composes with/preserves R2-15's proposed linear-walk semantics when that
lane is merged. The actual `bd9aa61` base still has the older per-ancestor
canonical-open implementation; this report does not claim otherwise.

The brief contains construct and file names but no inherited `file:line`
citations to classify. The broker constructor, worker error mapper,
project-instruction loader, and hook discovery sites were therefore grep-located
against the actual `bd9aa61` base before editing; no stale inherited line number
was treated as authority.

Territory overlap: this lane necessarily touches the retain lane's named
`session_hub/{actor,mod,rpc}.rs` territory, solely for the smallest
receipt-backed workspace mutation and publication seam. It does not alter
worker-manager retention, idle retirement, cache admission, or observation
retention logic.

## Contract and compatibility audit

- `ErrorCode::WorkspaceUnavailable` is additive; it serializes as
  `workspace_unavailable` and is explicitly distinct from `ProviderError`.
- `WorkspaceEventPayload` is an additive supplemental union. Existing core
  event variants and legacy fixtures are unchanged.
- Welcome advertises the additive `session_workspace_set_v1` feature. The
  request/response method matrix, client contract counts, enum audit, JSONL
  contract, and event-schema changelog were updated together.
- The exact JSONL notice golden pins one event, the path and `missing` reason,
  and `ui=true` / `durable=true` / `prompt=omit`.
- Valid-root shell lifecycle, protocol, RPC, store, CLI, and TUI goldens remain
  byte-for-byte green. No OAuth source or test was changed.
- The implementation uses `std::fs::metadata` and the platform directory-open
  seam; Windows behavior is covered by the existing platform abstraction and
  was reviewed, but the process-level deletion test was run on macOS only.
- Store schema v28 is a forward migration that rebuilds the hook outbox with
  the per-run fence columns and adds the derived unavailable-run table; fresh
  and upgraded schema shapes are pinned equal.

## Required behavior tests

| Requirement | Evidence |
| --- | --- |
| Delete root between turns, TUI-free | `vanished_workspace_degrades_plain_turn_and_workspace_set_replays`: real daemon process over UDS, fake provider; turn 1 succeeds, root is deleted, a fresh attach still succeeds, turn 2 still succeeds, and the provider receives the request. |
| Exactly one durable notice | The same process test asserts one `workspace_unavailable` event for the degraded run and no project-instruction fact, provider `RunFailed`, or effect receipt. |
| Typed classification/refusal | `workspace::tests::typed_error_is_not_a_provider_error` and `unavailable_workspace_tool_call_is_a_typed_rejection_without_an_effect`. |
| Missing/not-directory classification | `workspace::tests::classifies_missing_and_non_directory_roots`; unreadable maps through the directory-open failure path. |
| Re-root and replay parity | The process test selects a fresh root, completes a third turn, deletes that new root, then proves identical receipt replay before filesystem validation. |
| Root-authority and hook race fences | The process test proves an active turn rejects re-root with typed `busy`; `workspace_selection_atomically_fences_old_hook_dispatch_rows` pins atomic removal of old-root outbox rows and `previous_path` on the surviving selection fact. |
| Same-path restoration race | `unavailable_turn_pin_survives_same_path_restoration_without_hook_discovery_or_fire` exercises both the live synchronous pin and durable outbox fence, recreates the exact deleted path with hooks, and proves no discovery or hook fire occurs for that run. |
| TUI action and projection | `workspace_notice_is_visible_and_not_counted_unknown` clears stale state at the next turn opening; `retry_offers_current_cwd_then_maps_to_workspace_set_wire` pins the action and request. |
| CLI shorthand | `shorthand_requires_one_unambiguous_session` pins the literal shorthand's safe target selection. |
| JSONL golden | `workspace_unavailable_notice_jsonl_golden`. |
| RPC/schema parity | RPC exhaustive wire/method tests and protocol additive golden/schema-changelog tests. |
| Store migration parity | `migration_from_0_0_962_shape_matches_fresh_schema_exactly` and the full store suite cover v28 upgrade/fresh equivalence. |

## Verification record

All Cargo commands used the required environment:
`RUST_MIN_STACK=8388608`, `HAIDER_DISCOVERY_DISABLED=1`,
`HAIDER_TEST_DEVICE_NAME=test-mac`, `CARGO_INCREMENTAL=0`, and
`CARGO_PROFILE_DEV_DEBUG=0`; daemon-spawning tests additionally used
`HAIDER_TEST_SIBLINGS_PREBUILT=1`. `df -m /` was checked before builds/tests.
The final prebuilt `target/debug/haiderd` measured 185,663,424 bytes (>10 MiB).

- `cargo test -p haider-daemon`: **1,027 passed**, 3 live-provider tests
  ignored (922 library + 103 integration + 2 ancillary tests). One first pass
  saw the pre-existing timing-sensitive native-pipe coalescing test fail; its
  exact rerun and a complete clean suite rerun both passed.
- `cargo test -p haider-cli` with prebuilt siblings: **588 passed**, including
  121 process CLI tests and the one-shot daemon suites.
- Full `haider-protocol`, `haider-rpc`, `haider-store`, `haider-tools`, and
  `haider-tui` suites passed. The TUI run included existing exact goldens,
  10k-scroll coverage, and the 200k-shape benchmark.
- Scoped all-target Clippy for protocol, RPC, store, tools, daemon, daemond,
  TUI, and CLI passed with `-D warnings`.
- `bash scripts/qa-gate/run.sh test`: **64 passed**. This checkout has no root
  `run.sh`; `scripts/qa-gate/run.sh` is the maintained repository gate.
- `git diff --check`: passed.

There are no tests named literally `turngap` or `turnhygiene` in this checkout
outside the supplied evidence directories. Their relevant valid-root
invariants are exercised by the complete daemon/TUI suites and the QA
turn-performance harness; no claim of nonexistent literal test targets is made.

## CI error registry walk

| Registry class | Result |
| --- | --- |
| #1-#6 | Checked. The public additions are typed and additive, module visibility/imports are explicit, and exhaustive protocol/RPC/TUI projections compile and pass. |
| #7-#19 | Checked. No manifest, lockfile, lint weakening, or production dead-code escape was added. The scoped `too_many_arguments` helper allowance mirrors the adjacent journaling helper, and the test-only exhaustive enum matcher mirrors the existing schema matcher pattern; scoped all-target deny-warnings Clippy and `git diff --check` pass. |
| #20/#21/#48/#54 | Checked. New behavior has named tests in existing targets, none is ignored or platform-gated, and every Cargo run used the required 8 MiB stack. |
| #22-#28 | Fixed/checked. Store v28 migrates the hook outbox and adds the derived unavailable-run fence; fresh/upgrade shape parity and the full store suite pass. Provider authority, generic process execution, and Windows wire formats are unchanged; Windows filesystem behavior is by inspection. |
| #29-#44 | Checked. The real daemon/UDS deletion test covers fresh attach, three turns, typed busy mutation, re-root, and lost-response replay. No dependency, release, catalog, autospawn policy, or socket-root contract changed. |
| #45/#77 | Checked. No unsafe code or allowance was added; repository unsafe-count guards remain in the maintained QA gate. |
| #46-#63 | Fixed/checked. Runtime-root derivation and R2-15's secure-walk semantics remain unchanged. Cheap availability checks fence instruction, handoff, hooks, broker tools, receipts, and compaction; valid-root suites/goldens remain green. |
| #64/#67/#71/#72/#74 | Checked. The final prebuilt daemon is 185,663,424 bytes, daemon-spawn tests used prebuilt siblings, discovery stayed disabled, and test profiles/roots are isolated. |
| #65/#68-#78 | Fixed/checked. `workspace_unavailable` is distinct from `provider-error`, contains bounded path/reason detail, and is projected durably to TUI/JSONL. RPC/CLI recovery failures remain typed and OAuth files are untouched. |
| #79-#93 | Fixed/checked. Process ownership, output readers, PID identity, staged publication, sparse-file, line-ending, and sampling contracts are unchanged. The live plus durable hook fences have a named same-path restoration race test. |
| #94 | Checked. This lane adds no product deadline. Test waits use existing derived harness budgets; no literal timing shortcut was introduced. |
| #95 | Checked. No new external-state wait occurs while a negotiated connection is open; keepalive behavior is unchanged. |
| #96-#98 | Checked. Provider terminal-delivery reserve and route attribution are unchanged. Receipt-first workspace-set replay and the hook run fence preserve, rather than bypass, durable boundaries. |

No new CI error class was discovered.

## Independent verification

The closing verifier re-read the completed implementation and report and found
no release blocker. The audit explicitly covered typed classification and
JSONL/TUI projection, rootless plain-chat degradation and tool refusal, both
turn probes plus the broker-race fallback, live and durable same-path hook
suppression, atomic/replayable re-root, recovery surfaces, affected suites,
Clippy, QA, and diff hygiene. Verdict: `SHIP`.

SHIP
