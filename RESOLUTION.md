# Wave 973 integration resolutions

## ci-winpath integration on 2026-11-07

Integrates wave `b4db15478f938467ac3e1de6292cb96886d0be2f` into lane
candidate `ccbb96bfdac917f2cfdc04bc868a242150a8476b` with both histories
retained. The textual conflicts are this integration record and the shared test
baseline; both resolution histories are retained below.

The wave's catalog ownership, management-watch and saved-model settings changes
remain alongside the lane's session-owned display context. The merged creation
and summary handlers retain the catalog metadata and use the saved display home
for path labels. Actual workspace and origin paths retain their I/O authority.
The wave's macOS installer changes are retained without lane edits.

The merged source counts 7572 tests using xtask's exact file/marker rule.
The scheduled actual xtask update/check/count must confirm this value. No prior
candidate gate receipt is credited to this merge. The Sol finisher runs the
TARGETED tier; the separate Astra gate owns full workspace and packaging gates.

## ci-winpath integration on 2026-10-30

Integrates wave `5837b85ad9746a3a6adbbad74e1e0f23b106d566` into lane
candidate `882dca65b27f94e342bab58df2b5aeae4d7707d2`. The only textual
conflict is the shared test baseline. Both parents' source changes are retained.

The wave's fork prompt history keeps its subject session, owning agent and
source branch, including delayed ownership snapshots and manifest replay. Its
launch-origin revision handling and subject-session guard remain intact. The
merged summary handlers still project path labels through the model's saved
home context. Raw workspace paths and origin records retain their authority;
display labels do not replace I/O paths.

The merged source counts 7339 tests by xtask's file/marker rule: the prior lane
count of 7312 plus the wave's 27 new tests. The actual xtask update/check/count
must confirm this value under the build scheduler. Validation runs afresh on
the committed merge. Under the current owner gate-tier rule, the Sol finisher
runs TARGETED checks; the separate Astra gate owns the full workspace and
packaging matrix. No earlier candidate receipt is credited to this merge.

## ci-winpath integration on 2026-10-29

Integrates wave `183a99f63568100539b15d25ae4ceda34618c546` into the lane
including glob enclosure repair `d370acfc6c6a04b5fe33a70b09e5ab23df6f386b`.
The four textual conflicts were LiveDriver summary routing, the plain and rich
extension chains, and the test baseline. Both histories are retained by a merge.

The wave's complete-list/incremental-roster split is retained. The moved summary
handler continues using the model's home snapshot for path labels; the existing
context regression now exercises both reply kinds with own/other account rows.
Plain and rich extension rendering retains both helper-stop notices/results and
context-aware image projection. Other wave changes are preserved. No actual I/O
path is replaced by a display label, and this lane adds no daemon/store schema.

The merged source count is 7312 using xtask's exact file/marker rule; the
queued actual xtask update/check must confirm it. All native, platform, class,
mutation and live gates run afresh on the committed merged candidate. No receipt
from the earlier candidate is credited to this integration.

## ci-winpath integration on 2026-10-26

Integrates wave `b08f4d823d9aee1aee5a2dbd67acd557949da7a7` into
ci-winpath candidate `06459d9e5e0493d55f887f56d078401b0739e68b`.
The only textual conflict is `test-baseline.txt`: the merged source contains
7163 tests by xtask's exact source-count rule. The queued lint gate will
regenerate and check that value with the actual xtask executable.

The provider-budget and route-epoch changes in AppModel and LiveDriver are
retained alongside the lane's session-owned path projection. No new path
display producer was introduced by those changes. Startup home seeding,
plain/TUI path parity, the glob branch scanner, and SSH panel clearing are
preserved. Both histories are retained with a merge commit. No previous
candidate build or test receipt is reused for this integration.

The first local all-target Clippy check found that the lane's display-context
shell fixture omitted the wave's optional `setup_error` field. The fixture now
sets it to `None`, matching the other running-shell fixtures. Validation is
repeated on the repaired candidate.

## ci-winpath integration on 2026-10-24

Integrates wave `6fa2a72d4199b798834cee2d72c5fd2767a6373b` into
ci-winpath candidate `c1e5ae07e016c563c0a26dfaa369cfb306cd1607`.
The textual conflicts were this integration record and `test-baseline.txt`.
Both lanes' resolution histories are retained below. The merged source counts
7089 tests by xtask's `rust_files` / `is_test_marker` rule; the queued lint
gate regenerates and checks that value with the actual xtask executable.

The input-owner, terminal-read and SSH setup/history changes are retained.
The path-context projection, independent startup-home snapshot, and clearing
of both SSH panel footprints remain in place. No path label replaces an I/O
path. External review packages use the same vendored Crossterm patch as the
workspace, and the full local matrix includes the new Crossterm member.

## ci-winpath history


Merge commit 1b4be0208b4a58fca34ac125b55d3be0dda443a1 = lane 98ae98571df63bfb6410fcf74b258f2aee37c37d + origin/wave-973
56e4a29a3bfa58f422f705302b28b0a15648531b (fresh fetch 2026-10-23T16:2xZ, unchanged since Sol cap1). Merge commit, no rebase.

Textual conflicts: only test-baseline.txt (6575 lane vs 6625 wave). Neither describes the merged tree; regenerated with
the xtask counting rule to 6719 (the lint phase runs `xtask test-count` against it).

Semantic adaptations (no textual conflict):
- The wave added `activity: Option<ActivityStateV1>` to tool-call items. Lane-owned fixtures in plain_tests, toolview_tests
  and tui-exe display_path_tests set `activity: None`.
- image_created_tui_tests: the lane routes image rows through the session PathContext; the fixture now pins the session
  workspace and passes model.tool_path_context() to the plain renderer too (same context for styled and plain).
- toolview_tests: the broad-alias test asserts is_multiuser_workspace_root on the canonical alias and the filesystem-root
  alias, with a narrow-workspace negative control (kills the prior-broad-alias-lexical survivor from 16-opus-impl).

Wave path-producer audit: the wave's additions to haider-tui/src, haider-tui-exe/src, haider-client/src and
haider-cli/src (activity icons, fleet glyphs, stream-lock fix, update lock, terminal backend) add no path/cwd/home
display producers. The lane touches no daemon store, schema or protocol file.

Repairs carried in the merge commit (found by Sol finisher capture/source review):
- Startup home snapshot independent of a readable cwd (seed_display_dirs / apply_cwd_for_path take Option<&Path>).
- SSH profile list and form panels render Clear over their own footprint.

Sol finisher review (2026-10-24): retained the merged implementation and tests.
The startup-home change refreshes display homes even when `current_dir()` fails,
while preserving the model's existing directory context. Both SSH panels clear
only their own footprint; remote form values retain their exact spelling.
No store schema or daemon record format changed in this lane. Runtime and gate
results are recorded separately in the lane's evidence directory.


## Model list lane integration history

Merge of origin/wave-973 at b3aeb633 into lane-973-accounts-ux, round 23.

- accounts_tests.rs: preserve the second discovery release required by the lane's two account discoveries; use upstream's fixture-ready TEST_WAIT_TIMEOUT instead of the two-second load bound.
- race-stress-cases.tsv: retain both the lane's OAuth returned-issuer case and every upstream file-lock/fixture-readiness case.
- test-baseline.txt: derived as 6720 by the actual dependency-free xtask test-count --update command on the merged source. Receipt: state/evidence/973-accounts-ux/24-sol-r23-cap3/baseline-only.json (harness-relative). Final integrated source will be counted again after the newest upstream landing.
- Auto-merged production changes are reviewed together with both P2 repairs and gated on this integrated tree. No push is authorized in this round.

Fresh upstream integration: origin/wave-973 advanced to 56e4a29a3bfa58f422f705302b28b0a15648531b (macOS packaging fixes). Its four packaging/test files merged without source conflicts. The only conflict was test-baseline.txt; actual xtask test-count --update re-derived 6720 on the combined source (baseline-only.json). The original b3aeb633 merge and both account repairs remain in parent 5d62c65e.

Additional recovery repair before the final candidate: reproduced an interrupted sign-in being incorrectly committed when a different saved account had the same saved value. The named regression failed on unchanged production code (staged-before-proof.json), then passed with the repair. Recovery now honors a published target and, when present, the staged stable account coordinate; the existing matching rules stay unchanged. The regression covers 24 Anthropic/OpenAI combinations, including same-identity duplicate rows. The paired profile set adds a thirteenth pending same-value case. The repaired accounts suite passed 181 tests with zero failures and its two existing live-provider ignores (repaired-accounts-audit.json). Actual xtask test-count --update re-derived 6721 on this repaired source; the complete final-candidate gate counts it again.

Further recovery class repair: six named regressions actually failed on unchanged c2db6851 production (five existing A23 functions passed). Fresh interactive claims now protect their original alias in the claim transaction, and every interactive write stages its selected generation. Startup retains a published target, recognizes captured legacy identities, records the saved staged target before finalization, and leaves removal-reserved targets pending. Imports keep their existing coordinates. No schema, enum or stage format changed. The repaired Store library and accounts suites pass, including all eleven A23 functions, 62 recovery variants, six claim/control cases and four real actor failed-write boundaries. Actual xtask baseline: 6727. The paired real-daemon fixture set now has fifteen profiles, including email-only sign-ins and an unstaged fresh claim that must stay pending for both readers. Source/count receipts: state/evidence/973-accounts-ux/24-sol-r23-cap3/legacy-repaired-audit.json. Full exact-candidate verification follows this commit; no push.

Paired-profile fixture correction: the first real 13071bcd comparison passed all four original sign-in cases (21 assertions), but its constructed missing-Y profile was invalid before recovery: it deleted the selected Y and left X inactive. The new AccountStore loader assertion actually reproduced that failure. The checkpoint now represents a valid earlier projection with X selected, and recovery must restore Y while preserving X selection. Every exported profile is loaded and compared through the real AccountStore before the fixture test passes. No production, matching or persisted-format behavior changed. The complete Store/accounts suites and actual xtask baseline were rerun; baseline remains 6727. The real paired probe now preserves text daemon diagnostics, records a failed start separately from daemon-not-running cleanup, and explicitly checks rejection of the invalid no-active snapshot before exercising the valid missing-Y case. Exact fixture before/after evidence is under state/evidence/973-accounts-ux/24-sol-r23-cap3; full final verification is repeated on this new commit. No push.

## Round 24: complete OAuth recovery and placeholder matching

Recovery now recognizes a completed interactive OAuth write only when the saved bundle's entire canonical payload matches the command-owned stage. Stable identity, access value and generation can all recur across writes, so none is sufficient by itself. An incomplete write retains its pending receipt and stage without replacing a newer saved bundle. Published targets, reservations, removal history and separate import recovery retain their existing rules; no persisted format changes.

Account matching now rejects an Anthropic grant-hash placeholder on either side, including a legacy email bridge. This shared rule keeps fresh requested aliases distinct and prevents CLI/TUI duplicate-removal advice from treating a placeholder as a verified match. Explicit same-alias re-login remains allowed. OpenAI user-qualified identities with `verified=false` and ordinary normalized-email migration remain valid.

Durable tests include the real actor's failed-write boundary and completed controls, every accepted bundle field (including same-generation differences), historical/current stages and missing projections, symmetric placeholder/email matching, and CLI/TUI notices. The paired-binary and mutation evidence is recorded under the round-24 harness evidence directory.

---

# Wave 973 integration, Sol round 16

Merged `origin/wave-973` at `a8b218079422dd0a70c6997a56fc6a4f5d4b051f` into the model list fixes lane.

`crates/haider-daemon/src/android_runtime_tests.rs` conflicted because the lane had repaired the recovered-checkpoint fixture's effect history while the wave had extracted that fixture into `checkpoint_test_support.rs`. The shared helper already includes the same ordered Intent → Authorized → Dispatched → Outcome → CheckpointRecorded history, and asserts that the checkpoint is indexed. The resolution uses that shared helper and removes the duplicate local definition. It retains the lane's recovered Android RPC assertions for allowed paths, outside/reserved paths, worker generation, undo, redo, and rollback, along with the wave's general support for creation/write/deletion fixtures and durable postimages.

The shared helper's explicit fake-model session also initializes `model_catalog_key: None`, matching the lane's session-creation contract and preserving its configured-model control.

The imported activity-settings retry test used the former `(provider, model)` settings API. It now uses `LastModelPick` and retains all theme, notification, verbosity, dirty-intent and retry assertions. The same test checks legacy, configured and fetched saved picks, including their owner metadata, so an unrelated activity-setting write cannot discard saved model settings.

Parent bootstrap and both current composers use one account lookup that follows an exact session account pin. A pin bypasses default-account selection, so an account reply that changes the default must preserve an unchanged pinned producer while its matching catalog scope remains available. The pinned account's incarnation must still match; changing that producing account retires its former model. Four production-library cases cover parent and child composers during account-before-provider delivery, both unchanged and changed pinned producers, with both terminal sizes rendered. The round 16 evidence contains the immutable old-code comparison and the continuity mutation definition.

`test-baseline.txt` was derived from the merged source and the new regression tests using `cargo run --locked --offline -p xtask -- test-count --update`. The resulting count is **6,774**. Neither parent's number was chosen. The round 16 `integrated-baseline-update.log` records the actual command and exit 0.

The imported Android application changes are part of the wave. The model list repair adds no Android application UI change. Its standalone Rust feature, the shared fixture, and the full workspace gates are checked on the merged candidate.

A fresh wave read before the final gates advanced to `b3aeb633b9f24c51d07cb901bb532ad907b71782`. Its two conflicts are resolved semantically. The sidecar tests now join the writer before their final observations, and use the wave's complete seed-projection barrier while warming a live writer. The lane's committed-coverage helper and regression controls remain. The stalled-outbox fixture retains the lane's shared parking barrier, verifies that its first event fits the byte budget, waits for confirmed Busy admission, and then sends a deliberately oversized pressure event. This satisfies the wave's requirement to establish busy replay before applying pressure. Both sets of race-stress entries remain.

The second integration also imports the wave's recency fixture, which checks query work with metadata tripwires rather than a 500 ms scheduling bound, and its parent-exit liveness fixture. Their original behavior assertions remain. Final test-count derivation and isolated recency execution are recorded separately in the round 16 evidence.

After resolving the second merge and adding the two delayed-provider child controls, the actual xtask update derived an intermediate **6,812** tests. `expanded-baseline-update.log` records that command, its merged working-source hashes, and exit 0. The four exact-pin producer controls then brought the actual `xtask test-count --update` result to **6,816**. `pinned-baseline-update.log` records the working-source hashes, command and exit 0. The final committed count is checked again by the final gates.

Continuation on 2026-10-20 merged fresh wave `56e4a29a3bfa58f422f705302b28b0a15648531b`. Only `test-baseline.txt` conflicted. The imported macOS packaging changes remain intact. The existing verified xtask executable (unchanged xtask sources) actually re-derived **6,816** under the build-slot wrapper; neither parent baseline was selected. The continuation receipt is `17-sol-r16-cap2/gates/integrated-baseline.json`.

The continuation's complete matrix found that the source-test reachability guard could not see the partial-write module declared inside `accounts_tests.rs`, although its 15 tests had executed in the daemon library. The module is now declared directly in the existing `cfg(test)` section of `accounts.rs`. Its explicit fixture imports use account-module-only visibility. The guard, test bodies and baseline count remain intact; the failed receipt is retained and the repaired candidate is checked separately.


## terminal UI lane history

The round 9 merge integrates `a8b218079422dd0a70c6997a56fc6a4f5d4b051f` into the terminal UI lane based on `a97534c62a6005545df9796db68a475a14fc9ffd`.

In `session::chip_apply`, the envelope remains authoritative for the event timestamp and activity provenance, including replay, background and parked branch routes. The lane's question creation and matching answer/close updates remain before transcript reduction, so the previous open menu can still be matched. Neither side's behavior is discarded.

The keyboard input tests now construct real child envelopes instead of passing bare timestamps to the changed API. The baseline was re-derived as **6,950 tests** with `cargo run --locked -p xtask -- test-count --update` on the resolved tree. It is checked again on the committed merged candidate.

Round 9 also repairs visible child question input ownership, retires old keyboard input after accepted external editor writes, carries remote connection attempt identity through request/reply mapping and keyboard input handling, and conditions paste-specific decoder tests on the paste feature. The pre-existing overlay text observation F4 is deferred to lane 974 as instructed.

The continuation also integrates fresh wave `b3aeb633b9f24c51d07cb901bb532ad907b71782` after its file-lock and CLI lifecycle updates. The only new conflict is the shared stress-case catalog: retain all thirty appended wave rows and this lane's `tui-frame-publication` row, preserving the six-column format and unique case IDs. Place the frame-publication row with the existing TUI cases so future wave additions at the end do not conflict with this lane's appended row. The TUI repair code remains unchanged. The test baseline was re-derived as **6986 tests** from this integrated source before committing.

The next continuation integrates wave `56e4a29a3bfa58f422f705302b28b0a15648531b`. Its only additions since the prior merged wave are macOS installer identity checks and their Python tests. The sole merge conflict is the shared baseline; xtask re-derived it as **6986 tests** on the integrated tree. All Rust sources, Cargo manifests, Cargo.lock, vendor sources and Rust test drivers remain byte-identical to `6da30807`; source equivalence and evidence reuse are recorded with the continuation receipts.


Round 24 integrates fresh wave 6fa2a72d4199b798834cee2d72c5fd2767a6373b with a merge commit. The terminal lane's shared InputOwner and clipboard routes remain authoritative. The expanded accounts notice joins that model as AccountsNotice, with an opaque revision for replacement/reopen input stamps; its existing scroll/close keys and covered-input refusal remain. Pinned account forms and replacement confirmation retain roster paging. Both resolution histories are retained. Actual xtask re-derived the baseline as **7,106 tests** after resolution. Formatting, TUI all-target Clippy and six affected suites pass; full final gates run on the committed integrated candidate. The owner-agreement and covered-input matrices include AccountsNotice. No push.

---

## Catalog fixes round 17 integration

Merged wave `6fa2a72d4199b798834cee2d72c5fd2767a6373b` after implementing C5. Both lanes' resolution records remain above. The RPC public exports retain the catalog coordinate helpers and the incoming `ShellSetupErrorWire`. The merged baseline is derived by xtask before the merge commit; final gates run on that merge commit.

The actual merged `xtask test-count --update` derived **7198** source test markers. The round 17 `merged-preparation` receipt records the command and exit status.

The merged all-target clippy gate found two imported input-dwell fixtures missing the catalog fields introduced by this lane. Both now use the existing `metadata_provider` helper, which records their declared Static rows in `static_models`, leaves `configured_models` empty and has no fetched `catalog_scope`. All input assertions remain intact; source marker count stays 7,198.

Round 17 also carries the recorded coordinate into account-switch cache preflight. A raw creation alias otherwise reaches the exact pricing registry and unnecessarily requires an unknown-cost approval. A native wire-request regression covers the recorded alias, an exact-model control and an unlisted-literal control; it also checks that preflight preserves the public model spelling.

## Lockdown bindings continuation integration

Merged wave `7eebe6d09402823ea17b13fc498e5932e7361614` into the Lockdown bindings lane after its queued gates had not yet started. The stress catalog retains all eleven Lockdown rows and all four incoming catalog and sidecar-pressure rows, with unique six-column case IDs. The baseline is provisionally derived as **7,495** source test markers using xtask's exact traversal and marker rules; the queued xtask update and count gate must confirm it on the merged candidate.

All seventeen other Rust lane deltas remain identical to their pre-merge deltas against the earlier wave. The worker-retirement regression's session fixture adds `resolved_model: None` and `model_catalog_key: None` for the incoming model-coordinate contract. Its assertions remain intact. Save ownership, cancellation and replacement fencing, durable confirmation, startup joining, and the v1 ledger format retain the lane's behavior.

## Accounts round 26 integration

Merge wave `7eebe6d09402823ea17b13fc498e5932e7361614` while retaining the producing account on fetched and stale catalogs. The combined code keeps the wave's account incarnations, persisted model coordinates, catalog availability rules and management notifications. Admission and adapter reuse use the actual resolved account, including inactive pins and rotated accounts; the active-account flight guard remains separate from the original producer guard.

An inactive account's failed refresh records its own durable rejection without replacing another account's live inventory. Management publications carry the inventory owner and registry configuration together, so defaults and remote limits can be rebuilt for the requested account. Public and explicitly configured provider facts retain their existing behavior.

Both parents' regression cases remain. Test fixtures use the merged request and discoverer APIs, with their original assertions retained. The actual `cargo run --locked -p xtask -- test-count --update` on the combined source derived **7,654** tests. Final source, runtime and platform checks are recorded against the committed merge in the round 26 continuation evidence.


## Accounts round 28 wave merge

Merged wave `65e89efb076dd80fb96dd7caabcbb6f1d05aa4fa` into the accounts lane at
`8b8ad5cd2fd2a190db92be4252dfaeffc46cce03`, retaining both resolution histories.
Only this document and the shared test baseline conflicted. Product source
merged automatically; this round adds no product source changes.

The actual `cargo run --locked -p xtask -- test-count --update` derived **7,794**
test markers on the merged source. The common base derives 7,472, the accounts
lane adds 222, and the incoming wave adds 100: 7,472 + 222 + 100 = 7,794.
Both parents' tests are retained; neither parent's baseline was selected.
The round 28 evidence contains the update output and per-file count audit.
TARGETED gates run on the merge commit; Astra owns the subsequent FULL gate.
No push is authorized in this round.


## Lockdown bindings round 5 wave merge

Merged wave `a0335b35fe76fc7381fbd664c9d6502950ced848` into lane
`a1dc2228f646f8959451c0cdebdef37dfef2a175` with a merge commit.
Only this document, the shared race catalog and the test baseline conflicted.
Both resolution histories remain. The catalog retains every row from both
parents, including the eleven Lockdown settings-save/startup cases and the
wave's `daemon-oauth-flight-issuer` case: 133 unique IDs, six columns per row.
Product source merged automatically; this round makes no product source edits.

The actual `cargo run --locked -p xtask -- test-count --update` derived
**7,817** test markers on the resolved source. The common base has 7,472;
the lane adds 23 (7,495 total) and the wave adds 322 (7,794 total):
7,472 + 23 + 322 = 7,817. Neither parent's baseline was selected.
The round 5 evidence records the update output and per-file count audit.
TARGETED gates run on the merge commit; Astra owns the subsequent FULL gate.
No push is authorized in this round.
