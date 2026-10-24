# ci-winpath integration resolution (17-opus-impl)

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
