# v0.0.970 toolshape evidence

Branch: `lane-970-toolshape`. Work is intentionally uncommitted.

## Decision and implementation

Release verdict: **SHIP**. The complete 17-package gate on `7694ef9c` exited 0,
including doc-tests. Its sibling build, fixture regeneration, scoped Clippy,
QA harness, formatting, and unsafe-count checks pass. Independent review
confirms the integration preserves all incoming files and prior fixes, with
no new findings. The final upstream ref check still resolves to `7694ef9c`.

The locked truncation footer and typed `/truncation` mirror are implemented.
Pointers are relative to the durable `tool_result` payload. The existing nested
`result` also exposes the same additive fields for standalone result consumers.
No metadata serializes as `null`; unaffected legacy results omit it. SHA-256
covers original observed bytes before reduction or lossy UTF-8 conversion.
`payload_bytes` excludes the new footer and any separator it adds. Existing
preview contents and the former provider-bound cap/head-tail projection remain
intact; the footer is additional. Provider accounting includes its cost without
rewriting source-omission disclosures.

Applied filesystem results declare ordered `kind`, `name`, workspace-relative
`path`, `absolute_path`, and `bytes` from the same capture that produces the
checkpoint/workspace receipt and ledger paths. Moves expose source delete then
destination create/write. Byte sizes come from installed bytes or anchored
source metadata; directory structural operations use zero. Post-apply failures
retain effects, original error text/disposition, and fatal run behavior.

Process, filesystem read/search/glob, web fetch/search, SSH, bounded model
catalogs, task output, and delegated report producers carry provenance. Task
completion retains optional `output_sha256` across eviction/adoption. Unknown
legacy task hashes remain absent. Delegated recollection derives a long report's
original digest from its child journal. No wait/cancellation policy changes. Lockdown post-replace failures also
retain effects across directory sync/quota persistence and later event writes.

The public run contract and event changelog document every additive field and
byte-count scope; schema version remains 1. No persistence boundary is moved.
A fatal post-apply error now records the landed effects in a failed tool-result
fact before the existing fatal run cleanup.

## Required upstream integration

The prescribed fetch/merge commands, including the final retry, were blocked by sandbox restrictions on
`haider-agent/.git/worktrees/lane-970-toolshape/{FETCH_HEAD,ORIG_HEAD.lock}`.
At the earlier gate's start, HEAD and the available fallback both equaled
`b9c2a0475214102d1fb4c8d9c3ae3f480fd05fe4`. During that passing gate,
`origin/wave-970` advanced to `2ef44708757e0f87b4437ec4ab1594c6a680814e`
(toolrepair). A shared temporary checkout at that commit received the lane's
saved binary diff through `git apply --3way --index`. Actor conflicts retained
both invalid-call/corrected-name behavior and the new empty metadata defaults;
the test baseline is recounted by the repo tool. Resolved files were copied back
without changing this worktree's Git metadata. HEAD remains at `b9c2a047`.
The orchestrator owns recording the merge parents and commit.

The upstream-added `filesystem_aliases.rs`, `filesystem_edit_diagnostic.rs`,
and `docs/testing/v0.0.970/toolrepair.md` are present (untracked relative to the
old index), so the reconstructed merge does not drop upstream-added files.
The merged instruct-pipe pin is **12,764 -> 13,552** bytes; the increase comes
from upstream aliases/descriptions. Toolshape itself changes no input schema.
The provider-request golden is regenerated through its repo test helper.

The same temporary-checkout workflow then integrated
`f1cf80c9238bfe5b014e61b5e406723c38fa6e5d` (runperm), preserving all 114 affected
paths and every upstream-added file. The worker conflict keeps upstream's
permission error classification (using the wrapped source error) together with
toolshape's applied effects. The run contract retains both independent sections.
This integration changes headless JSONL fixtures through upstream policy; the
turnhygiene and oneshot repo helpers regenerate them before the final no-bless gate.
The source tree contains `f1cf80c9` plus toolshape. All four regenerated fixtures
are byte-identical to that upstream commit. The prior `2ef44708` run completed
its ordinary tests, but its last doc-tests saw source files from the advancing
integration and stale dependency artifacts. The fresh sibling build and full
`f1cf80c9` gate supersede that mixed-artifact run.

The passing `f1cf80c9` gate's final ref check found
`7694ef9cbd2fbbcedb24fee14dbf4b12b1c4cd39` (winclip). Its ten incoming paths
have no local toolshape overlap except the recounted test baseline. Every
existing incoming file was verified byte-identical to `f1cf80c9` before all
ten files were copied from the new upstream commit. The same metadata
restrictions prevent recording this merge; the resolved source now contains
`7694ef9c` plus toolshape. Native Windows clipboard execution remains by
inspection on this macOS host; upstream's Windows CI gate is preserved.

## Citation audit and territory

Read `LANE-COMMON.md`, `LANE-BRIEF-toolshape.md`, round-1 facts/proposal and the
relevant round-2 facts, lens tables and trace findings before implementation.
The brief has no product file:line citations. Old tool-settlement citations in
`turnperf/MERGED.md` (actor 8915) and `turnperf2/D7.md` (actor 9056/9166/9211)
are **drifted, construct-correct**: locate `commit_tool_settlement` and its
`EventPayload::ToolResult` batch in current actor.rs. That boundary stays intact.
No latency estimates from the historical evidence are treated as measurements.
The current process cap is 2 MiB, not the older 1 MiB research assumption;
the requested 1 MiB case truncates at model-result reduction.

Small cross-territory changes: `worker.rs` formats metadata and carries applied
failure effects; `delegation.rs` carries a summary digest and re-derives it on
recollection, without changing waits; `tasks.rs` retains output digests;
`lockdown/mod.rs` records whether a write landed before a later failure. Existing
BoundedResult/TaskCompleted constructors in consumers and tests gain explicit
empty defaults. OAuth files are untouched. Linux/Windows behavior is **by
inspection**; execution below is macOS only.

## Verification

All Cargo commands use RUST_MIN_STACK=8388608, HAIDER_DISCOVERY_DISABLED=1,
HAIDER_TEST_DEVICE_NAME=test-mac, CARGO_INCREMENTAL=0, CARGO_PROFILE_DEV_DEBUG=0.
Each build has a disk preflight above 700 MiB; builds use explicit packages.
Daemon tests use prebuilt siblings and HAIDER_TEST_SIBLINGS_PREBUILT=1.
Merged prebuilt haiderd is 198,776,064 bytes (>10 MiB); haider is 109,892,912 bytes.
`cargo run --locked -p xtask -- test-count --update` recounts **4,788 -> 4,866**
(merged upstream baseline 4,836 plus 30 toolshape tests).

- `bash scripts/qa-gate/run.sh test`: PASS, 65 tests (repository run.sh location).
- Initial `cargo check -p haider-daemon -p haider-cli --tests`: PASS.
- Protocol toolshape contract tests: PASS, 10; fixtures generated with
  `UPDATE_FIXTURES=1`, then PASS in the final normal no-bless run.
- Initial filesystem/process integration suites: PASS (19 + 29 + 29 + 7 tests),
  prior to additional task/post-apply failure coverage.
- Scoped `cargo clippy --locked` on protocol, tools, provider, core, daemon,
  RPC, daemond, CLI, TUI and store with `--all-targets -- -D warnings`: PASS.
- Changed Rust files: `rustfmt --edition 2024 --check`: PASS, 88 files.
- `bash scripts/check-unsafe-counts.sh`: PASS, production=189, test=20.
- Merged instruct-pipe golden: PASS at **12,764 -> 13,552** bytes.
- Final merged daemon library: PASS, 1,055 tests plus three pre-existing ignored
  tests. The 1 MiB process golden, SSH raw-byte provenance, delegated report
  recollection, task eviction/adoption, and post-apply lockdown fault tests pass.
- Final `haider-daemond` core-loop RPC suite: PASS, 20 tests, including the
  capped process result and its continuation.
- Initial full gate exposed two legacy assertions that included the new footer
  in a payload cap/JSON parse, plus an SSH fixture aligned exactly with the
  capture cap. Tests now verify the unchanged payload and footer separately;
  the SSH fixture checks raw invalid UTF-8 bytes at the cap against the retained
  lossy text, without assuming the transport observed unread overflow. No
  production behavior or previous cap was relaxed.
- The `7694ef9c` fixture-regeneration suite passes all nine tests, including
  provider requests, JSONL text/tool turns, and live/replay/detached parity.
  The oneshot JSONL regeneration test also passes.
- Existing JSONL goldens remain unchanged by toolshape. All four regenerated
  CLI fixtures are byte-identical to `7694ef9c`'s fixtures.
- All new toolshape goldens pass without blessing in the final merged gate.
  Ordered effect/receipt parity, post-apply failures, invalid UTF-8 original
  hashes, discarded-byte mutation, and corrected-name provenance also pass.
- Complete final `7694ef9c` crate gate: **PASS, exit 0**, including all 17
  packages and doc-tests. All 333 result summaries are successful. The existing
  large-transcript shape benchmark passes in 479.28 s.
  No tests were weakened and no ignores were added. Linux and Windows remain
  by inspection; execution is macOS only.

The final gate output is `/tmp/toolshape-7694-workspace.log`, with
`WORKSPACE_EXIT=0` in `/tmp/toolshape-7694-gate-status.log`. Supporting outputs
are `/tmp/toolshape-7694-clippy-final.log`, `/tmp/toolshape-7694-qagate.log`,
`/tmp/toolshape-7694-unsafe.log`, `/tmp/toolshape-7694-count.log`, and the
`/tmp/toolshape-7694-{fixture,oneshot}-regeneration.log` files. The index is
unchanged and OAuth files are untouched. The orchestrator must record the
resolved merge and commit.

The complete gate names all workspace packages explicitly, with the environment
and sibling preparation above:

```sh
cargo test --locked --no-fail-fast \
  -p haider-platform -p haider-protocol -p haider-accounts -p haider-core \
  -p haider-pdf -p haider-provider -p haider-daemon -p haider-daemond \
  -p haider-rpc -p haider-tui -p haider-cli -p haider-store -p haider-tools \
  -p haider-client -p haider-verify -p haider-stt -p xtask \
  -- --test-threads=4
```

Named proof includes `one_mib_stdout_golden_preserves_legacy_payload_and_pins_original_digest`,
`digest_changes_when_only_discarded_original_bytes_change`,
`toolshape_one_mib_stdout_hashes_original_before_model_truncation`,
`toolshape_one_mib_process_result_golden_preserves_payload_and_replays`,
`toolshape_fixture_write_result_golden_is_additive_and_replays`,
`toolshape_file_effects_match_receipts_and_ledger_in_effect_order`,
`toolshape_post_apply_failure_preserves_effects_and_original_failure_receipt`,
`toolshape_fatal_applied_effects_are_journaled_once_before_errored_and_replay`,
`toolshape_task_output_original_hash_survives_completion_eviction_and_adoption`,
`toolshape_collect_and_recollect_long_utf8_report_hash_original_child_journal`,
`toolshape_lockdown_quota_publication_failure_marks_the_already_applied_write`,
`toolshape_ssh_hashes_original_bytes_before_lossy_utf8_and_output_cap`,
`repaired_tool_name_preserves_declared_json_and_text_result_provenance`,
and the typed model-projection cap/savings/replay regressions in actor_tool_result_tests.

## Independent verifier value

Pre-finish review found V1 web-fetch overflow hashing, V2 delegated truncation
propagation, V3 post-apply effect loss, V4 model-cap bypass, and V5 omission
accounting loss. Each changed production code and/or a regression test.
Earlier protocol review also found lost filesystem savings and the missing
process-name trust guard; both are fixed and independently pinned.
The upstream merge review found V8: corrected tool names wrapped the footer as
JSON and invalidated its byte count. Correction now operates on `payload_text`
and redeclares the original provenance afterward; the regression also preserves
file effects and verifies one final footer.
Independent code/proof re-review after the `f1cf80c9` merge: **SHIP**. The
permission classification, applied effects, both contract sections, and every
incoming path are retained. Research and protocol merge sweeps found no further
issues. The final evidence audit confirms all named proofs, fixture equality,
test count, and contract wording with no new findings. Combined review:
**findings=8, real=8, noise=0**. Independent review of the subsequent winclip
integration also returns SHIP with no new findings. The full `7694ef9c` gate
passes, including the incoming clipboard/input tests executable on macOS.

## CI error registry walk (§A–§D)

All 1–95 classes were walked against this lane (taxonomy in the existing
968 ci-prep reports). No new class is introduced.

- #1–18: added types/default fields, exhaustive error matching, SHA2 imports and
  locked dependency entries; no new unsafe/unwrap production path or lock held
  across an await. Scoped compiler/Clippy checks recorded with final gates.
- #19–21: Rust 2024 formatting, actual test-count recount, required 8 MiB stacks.
- #22–37: no tracing/subscriber, migration, model discovery, runtime-root,
  autospawn, release, Android or dependency-feature-policy changes. Platform
  branches carry equivalent capture-derived metadata; Windows is by inspection.
- #38–54: streaming hashes add constant memory; search still enforces its hard
  result cap; originals/receipts are captured before mutation can race; no
  descriptor sweep or inode-reread inference. Goldens normalize only temporary
  workspace coordinates. Unsafe guard, all affected tests and siblings required.
- #55–73: no platform-gated weakening. Named golden, byte mutation, post-apply
  fault, live/replay and original invalid-UTF8 hash assertions. Sibling size and
  prebuild flag checked. Original error classification is retained.
- #74–93: additive fields documented and old readers default correctly. No
  credential/discovery, process ownership, detach, cancellation, exit-observer,
  epoch-fence, staged publication, maintenance or group-commit policy changes.
- #94–95: no production deadline, timeout, or sleep added; the additional
  recollection read uses the existing asynchronous journal path and existing
  budgets/keepalive servicing remain authoritative.
