# computeruse — v0.0.970

Base / merge: lane and fetched origin/wave-970 both `ea0b9a6b`.
The supplied worktree's Git metadata is outside the sandbox; fetch failed on
FETCH_HEAD and merge failed on ORIG_HEAD. A writable shared clone at
`/private/tmp/haider-computeruse-merge` fetched the real origin and ran
`git merge --no-commit origin/wave-970`: already up to date. Final changes are
exported as a commit/bundle without trailers or a push. Supplied LANE files and
turnperf/turnperf2 evidence are excluded from the commit.

## Root causes and scope

**Orientation — confirmed by executed synthetic CoreGraphics tests.**
`MacOsComputerBackend::capture_png_blocking` drew a CGImage into an RGBA bitmap
with a translate(height), scale(1,-1) CTM. Unlike a UIKit drawing surface, this
bitmap conversion already preserves CGImage scanline order. That extra transform
vertically reversed the PNG; it did not rotate the physical screen or swap x.
The fix removes the extra CTM at the source and extracts the same CGImage-to-PNG
conversion for the native capture and the synthetic test.
`synthetic_cgimage_png_preserves_orientation_edges_and_accessibility_coordinates`
creates a real in-memory CGImage with red TOP-LEFT, blue BOTTOM-RIGHT, a hard edge
and an integer-aligned arrow-shaped marker. It decodes the production PNG and
checks every byte, plus Retina image→Quartz→AX corner coordinates. It passed.
Restoring the old CTM made that same test fail (mutation executed and reverted).
No Screen Recording or Accessibility permission was needed or requested.

**Sharpness — source inspection plus executed synthetic admission test.**
Native macOS capture retains CGImage backing dimensions, including Retina 2x;
there is no hard-coded 1x rasterization and no JPEG round trip. Shared CU-1 CAS
admission caps the longest dimension at 2048 and PNG size at 5 MiB. Its former
Triangle filter softened edges; it now uses Lanczos3. Each size attempt starts
from the original pixels, avoiding cascaded resampling, and final dimensions
remain authoritative for clicks. `admitted_desktop_edges_keep_contrast_and_pixel_location`
passes a 4096px black/white edge through PNG→CAS admission→PNG decoding, asserts
the edge remains at x=1024 and its immediate neighbors retain at least 235/255
white and at most 20/255 black. The admitted metadata is checked against CAS.
This shared image-admission filter also serves other image results; no mobile
action/backend code changed.

There is **no pointer/arrow overlay compositor in the inspected computer path**.
The synthetic arrow proves captured marker pixels survive conversion exactly;
it does not prove a separate UI overlay is sharp or implement a new overlay.
There is therefore no existing overlay to move after scaling or align. The
owner's reported arrow appearance still needs the permission-enabled installed
visual check; we cannot establish its external renderer/root cause here.

Region zoom retains native crop pixels before admission instead of enlarging a
previous 2048px image. Redaction runs against the entire original screenshot
before crop, encoding, CAS publication, attachment, or graph evidence. Native
input coordinates and cursor/AX bounds use the exact outward-rounded crop,
including origin offsets and fractional screen points. Inspection after a crop
must return bounds mapped to its newly delivered full screenshot; this was
caught by independent review and corrected before verdict.

The effect classes remain the repository's `ScreenObserve` / `ScreenControl`
(the brief calls control ComputerControl). The broker, OS preflight, ownership,
redaction policy and cancellation semantics retain their existing paths.
Existing action JSON remains valid; region is an optional screenshot field and
triple_click is additive. No mobile action was added or changed.

## Action parity

| Action | Haider before lane | Gap | Lane fix |
|---|---|---|---|
| Full screenshot | Native PNG at backing resolution, CU-1 admission | macOS orientation / soft CU-1 resize | Source orientation regression and sharper admission (root lane evidence) |
| Region / zoom | Full display only | No region observation or input origin | Optional `screenshot.region={x,y,width,height,reference_width,reference_height}` crops a full-display reference after full-frame redaction, preserves native pixels, maps subsequent input to crop origin/extents |
| Left / right / middle click | All supported; right/middle at current cursor | No action gap | Schema explains `mouse_move` before current-cursor clicks |
| Double / triple click | Double supported | Triple missing | Additive `triple_click`, native click states 1/2/3 on macOS, three complete button cycles on Linux/Windows/Wayland |
| Click and drag | `left_click_drag` and held-button actions | No action gap | Existing mapping applies region origins |
| Scroll | Four directions + positive line amount at x/y | No action gap | Existing mapping applies region origins |
| Mouse move | Delivered image coordinates | No action gap | Existing mapping applies region origins |
| Type text | Unicode text, bounded length | No action gap | Unchanged |
| Key press / modifier chord | `key.keys` supports cmd/ctrl/alt/shift plus final key | Prose vague; malformed empty chord segments accepted by some backends | Clarify `+` syntax and sequential key actions; reject invalid modifier/empty segments before dispatch; native unsupported final keys remain typed errors |
| Wait | 0–60000 ms, cancellable | No action gap | Unchanged |
| Cursor position | Delivered image coordinates | Region inverse mapping missing | Crop-aware native→image position on supported backends; existing Wayland cursor support limitation remains |
| Retina image→screen | Full-display ratio mapping | Region offset/fractional portal logical dimensions | macOS crop point bounds; Linux root offset; Windows virtual-screen offset; Wayland floating logical origin/extents from source ratios |

Generic computer tool users (including the owner’s GPT configuration) receive region schema. Native Anthropic substitutes its own computer schema and does not advertise this custom region form. Replay explicitly rejects a region it cannot represent instead of silently dropping the crop. Existing native actions, including additive triple-click translation, retain normal behavior.

## Synthetic tests added by parity work

- `triple_click_is_additive_control_gated_and_rejects_coordinates`: tagged shape, fixed count, ScreenControl class, invalid extra arguments.
- `key_chords_keep_modifier_combos_in_the_neutral_contract`: four modifier combinations plus invalid/missing/string-array and malformed chord validation.
- `region_pixels_map_to_offset_x11_root`: offset origin, 2x scale, center, out-of-image rejection.
- `region_preserves_fractional_portal_logical_coordinates`: 2x backing image with odd crop coordinates and sizes preserves .5 logical points.
- `native_computer_action_translation_covers_supported_anthropic_vocabulary`: existing translation table extended with triple-click.
- `native_computer_replay_never_silently_drops_region`: unsupported native replay fails closed.
- Region agent adds validation, broker serialization, exact crop/Retina rounding, redaction preservation and daemon CAS/provider-delivery tests (see final test logs).

These tests do not post real input or request Screen Recording. Linux/Windows real capture and Wayland portal behavior are inspected, not executed on this Mac. Windows GDI uses negative `biHeight` to produce top-down BGRA and converts each row directly to PNG. X11 respects scanline stride and channel masks, retaining source row order and native dimensions. Neither uses JPEG. Configured redaction still covers the original full source pixels before any crop, admission, attachment or graph evidence.

## Pins / tooling

Computer compact schema: 1184 → 1955 bytes (+771 for region fields, triple_click and actionable descriptions).
Full authorized catalog: macOS 20770 → 21541; Linux 20819 → 21590; Windows 20769 → 21540; other Unix 20764 → 21535. Platform deltas preserved. Instruct-pipe invariant 6166 → 6166; default manual bytes 0 → 0 because computer remains a discovered stub. The on-demand manual now names region and triple-click.

On-demand computer manual line: 268 → 358 bytes; it is only delivered on discovery.

The computer manifest test now supports deliberate `UPDATE_FIXTURES=1` regeneration; run it once with that flag, then normally. The CLI provider-request golden uses the existing `UPDATE_FIXTURES=1` turnhygiene tooling. Root owns merged-tree execution and final measurement.

## Inspection boundaries

Read lane brief/common, turnperf facts and round-two C4 lens evidence. Their historical paths/line references describe latency architecture rather than computer behavior; no out-of-scope optimization was made. A recursive search of the checked-in `scripts/qa-gate` found no computer-specific action/capture checks; TUI alternate-screen/palette checks concern terminal rendering, not OS screenshot orientation. Do not claim the advisory real-screen gate ran.


## Required verification / executed versus inspected

ENV LAW for Rust commands:
`RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_TARGET_DIR=/private/tmp/haider-computeruse-target`.
Every build is preceded by `df -m /`; available space stayed above 700 MiB.
Daemon tests additionally use `HAIDER_TEST_SIBLINGS_PREBUILT=1` after prebuilding
haider, haiderd, and haider-tui. Full gate results and final artifact identity
are recorded below when complete.

Synthetic CGImage and CAS sharpness tests executed successfully. The old CTM
mutation failed as expected. No live screen was captured and no input events
were posted. macOS native input/TCC behavior and Linux/Windows/Wayland native
behavior are by inspection. Existing ignored hardware-only tests were neither
unignored nor newly ignored. Owner follow-up after install: `look at my screen`,
verify menu bar/top and Dock/bottom, then pointer sharpness, region/zoom and a
click into the indicated control on Retina.

## CI registry walk

Relevant checks: #10 warnings/dead code, #19 formatting/diff, #20 test-count
recount, #33 additive schemas and golden regeneration, #64 sibling binaries
(haiderd >10 MiB), #72 discovery disabled, #73 no fixed source windows, #94 no
new deadlines, #95 no negotiated-connection waits changed. Unsafe test count
haider-tools 4→5 for the permission-free synthetic CoreGraphics fixture;
production unsafe count stays 76. All other registry surfaces are unchanged:
no OAuth, store durability/sync, retry, recovery, delegation, monitor, installer,
network, or session-lifecycle semantic changes. The small daemon worker change
is confined to computer image admission and inspection result finalization.

## Independent verifier

Four findings, all real and fixed; zero rejected/noise findings:
Wayland crop pixels needed logical-point scaling; native Anthropic replay could
drop a crop; macOS omitted the accepted super modifier alias; inspect-after-crop
returned AX bounds in the preceding image. Final independent code review: SHIP.
Verifier ran no builds. Emergency/failure AX cleanup is best-effort so it cannot
prevent held-input release or the existing failure journal. Full gate is separate.

## Gate execution ledger

- Sibling prebuilds passed; refreshed haiderd measured 203,362,048 bytes
  (>10 MiB). No installed binaries changed.
- `UPDATE_FIXTURES=1` computer manifest regeneration passed; normal tool-crate
  tests also passed. Computer schema measured 1955 bytes.
- `UPDATE_FIXTURES=1` provider-request golden test passed; the regenerated
  provider_request_no_budget.json had no diff (computer stub unchanged).
- `xtask test-count --update`: 5135 → 5150. This is the static source baseline,
  not a claim that every platform-specific test executed on macOS.
- Formatting, diff whitespace, and unsafe-count checks passed.
- Initial full workspace run retained one existing autospawn timing failure:
  `parent_exit_leaves_the_daemon_running`, launch 1.086452s against the unchanged
  950ms deadline. The no-fail-fast run continued; no timing assertion was weakened.
  Final results/retry evidence follow below.

### Final gate result: NO_SHIP

The full merged-tree command `cargo test -q --workspace --no-fail-fast` completed
with exit 101. Its **only failed target** was
`haider-cli --test autospawn_tests`, test
`parent_exit_leaves_the_daemon_running` at autospawn_tests.rs:1064.
All other workspace targets, including computer/tool/provider/store/daemon
regressions, passed. The unchanged UI benchmark completed in 485.74 seconds.

The isolated retry under the same ENV LAW also exited 101: launch took
**1.560685542 seconds** against **950 ms** (first full attempt: 1.086452s).
The queued second full run was therefore not started. This is repeatable in
this environment, not a dismissed one-off flake. The autospawn test, launcher,
and startup implementation were not modified by this lane; no CI flag,
deadline, ignore, or test assertion was changed to bypass it. The exact landing
blocker is the pre-existing local autospawn launch-time gate; the orchestrator
must resolve/qualify that startup timing failure and rerun the full gate.

`cargo clippy --workspace --tests -- -D warnings` passed (exit 0), as did
prebuild, static test-count, formatting, whitespace, unsafe-count, the manifest
golden regeneration, the provider-request golden regeneration, and the focused
computer/tool and synthetic capture/sharpness tests. Final independent **code**
review was SHIP with four real findings fixed; final **lane** verdict is NO_SHIP
because the required workspace gate remains red.

Logs are retained in `tmp/computeruse/` with the bundle. An attempted read-only
process sample of the slow UI benchmark was denied by the sandbox; no privileged
retry was made. It subsequently completed normally. No environment disk guard
was triggered; all build starts had more than 700 MiB available.
