# Wave 973 integration resolutions

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

## terminal UI lane history


The round 9 merge integrates `a8b218079422dd0a70c6997a56fc6a4f5d4b051f` into the terminal UI lane based on `a97534c62a6005545df9796db68a475a14fc9ffd`.

In `session::chip_apply`, the envelope remains authoritative for the event timestamp and activity provenance, including replay, background and parked branch routes. The lane's question creation and matching answer/close updates remain before transcript reduction, so the previous open menu can still be matched. Neither side's behavior is discarded.

The keyboard input tests now construct real child envelopes instead of passing bare timestamps to the changed API. The baseline was re-derived as **6,950 tests** with `cargo run --locked -p xtask -- test-count --update` on the resolved tree. It is checked again on the committed merged candidate.

Round 9 also repairs visible child question input ownership, retires old keyboard input after accepted external editor writes, carries remote connection attempt identity through request/reply mapping and keyboard input handling, and conditions paste-specific decoder tests on the paste feature. The pre-existing overlay text observation F4 is deferred to lane 974 as instructed.

The continuation also integrates fresh wave `b3aeb633b9f24c51d07cb901bb532ad907b71782` after its file-lock and CLI lifecycle updates. The only new conflict is the shared stress-case catalog: retain all thirty appended wave rows and this lane's `tui-frame-publication` row, preserving the six-column format and unique case IDs. Place the frame-publication row with the existing TUI cases so future wave additions at the end do not conflict with this lane's appended row. The TUI repair code remains unchanged. The test baseline was re-derived as **6986 tests** from this integrated source before committing.

The next continuation integrates wave `56e4a29a3bfa58f422f705302b28b0a15648531b`. Its only additions since the prior merged wave are macOS installer identity checks and their Python tests. The sole merge conflict is the shared baseline; xtask re-derived it as **6986 tests** on the integrated tree. All Rust sources, Cargo manifests, Cargo.lock, vendor sources and Rust test drivers remain byte-identical to `6da30807`; source equivalence and evidence reuse are recorded with the continuation receipts.
