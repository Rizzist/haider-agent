# Lane 971-UI-fleet — subagents and the descendant fleet

Its own report file: the shared `android/app/REPORT-971-UI.md` does not exist on
this branch's base (`lane-971-ui @ ff3c1e1a`), and round 12 is writing to it on
`lane-971-ui` in parallel. This lane keeps its own; the two are concatenated at
integration.

Base: `lane-971-ui @ ff3c1e1a` (round 11). Candidate: the tip of `lane-971-ui-fleet` (not pushed). The gated source tree
is `android/app/src` = **`7319a0a7e426c176da1db29570670aa48f175228`**, quoted rather than the commit sha because
amending this file changes the commit and not one byte of the tested source. The gate below ran on exactly that tree. Implementation model: **Opus 5** (UI).
Code verification, device verification and SHIP are **GPT6-Astra's**, in a
separate context; nothing below claims a device observation.

## Round 1 — parity with the desktop fleet surface

### Wire truth read first

| Fact | Source |
| --- | --- |
| `parent_session_id` on a roster row | `crates/haider-rpc/src/frame.rs:1949` |
| `session.fleet` request / response | `frame.rs:3495`, `frame.rs:4699` |
| `SessionFleetSnapshot` | `frame.rs:2395` |
| `FleetNodeWire` (incl. `folded_children`) | `frame.rs:2321` |
| `FleetRollupWire` / `FleetStateCountsWire` / `FleetMetricsTotalsWire` | `frame.rs:2382`, `:2358`, `:2369` |
| `FleetAgentStateWire` (`#[serde(other)] Unknown`) | `frame.rs:2306` |
| `ObserveSubagentWire`, carried at `SessionObserveDigest.subagents` | `frame.rs:2219`, `:2271` |
| `lockdown_bound` / `lockdown_auto_hermetic_bound` are `#[serde(skip)]` | `frame.rs:2232-2239` |
| `LockdownStatusWire` | `frame.rs:1344` |
| `AgentMetricsSnapshot` / `AgentUsageMetrics` | `crates/haider-protocol/src/agent.rs:196`, `:219` |
| `session_fleet_v1` feature gate | `frame.rs:395` |
| Android keeps `SpawnSubagent`/`MessageSubagent` in-process | `state/971-CONTRACTS.md:73` |

Desktop reference ported (read-only): `diffforge-client/src/sessions/` —
`fleetModel.js`, `useFleet.js`, `FleetPanel.jsx`, `FleetChildTranscript.jsx`.

### What landed

**Facade (additive; no existing member changed).**

- `ui/daemon/FleetFacade.kt` — `FleetAgentState`, `FleetStateView`, `FleetNode`,
  `FleetRollup`, `FleetSnapshot`, `FleetNodeMetrics`, `FleetUsage`, `Subagent`,
  `SubagentLockdown`, `FleetLoad` (`Unread`/`Loading`/`Snapshot`/`Unavailable`/
  `Failed`), `FleetCompleteness`, the pure `FleetModel` transforms, and
  `FleetRpcAdapter`, which parses the real frames with absence preserved.
- `ui/daemon/DaemonFacade.kt:426,435` (`SubagentLoad` at `:440`, the reason
  constants at `:451`) — `DaemonService.fleet()` / `.subagents()` with
  **default bodies** returning `Unavailable(FLEET_NOT_WIRED)` /
  `Unavailable(SUBAGENTS_NOT_WIRED)`, so lane 971-3's `RpcDaemonService` still
  compiles after the merge and an unwired seam is never mistaken for "this
  session has no subagents"; plus `SubagentLoad`.

**Drawer nesting.** `ui/state/SessionTree.kt` — `nest` `:92`, `summarise` `:127`,
`familyTier` `:155`, `orderRoots` `:171`, `captureOrder` `:203`, `sections` `:227`,
`defaultExpanded`/`expanded` `:279,286`, `lines` `:290`, `COLLAPSE_THRESHOLD` `:39`
— built *on* `SessionListState`, not replacing it.
`ui/drawer/SessionTreeRow.kt` — the drawn indent rail + connector elbow and the
48 dp family control carrying the descendant count and the aggregate dot.
`ui/drawer/SessionDrawer.kt:141` (sections), `:263` (lines), `:151,305,457`
(the "Agents (N)" footer row, only when the roster carries delegated sessions).
`ui/chat/ChatViewModel.kt:184` — the freeze captured with
`SessionTree.captureOrder`.

**Header chips.** `ui/fleet/SubagentStrip.kt`, mounted at
`ui/scaffold/HaiderApp.kt:418`.

**Child transcript.** `ui/fleet/ChildTranscriptScreen.kt`, routed as
`Overlay.ChildTranscript` at `ui/scaffold/HaiderApp.kt:213-241`.

**Fleet panel.** `ui/fleet/FleetPanel.kt` (`FleetPanelModel` +
`FleetPanelContent` + `FleetSheet`), routed as `Overlay.Fleet` at
`ui/scaffold/HaiderApp.kt:592`. `ui/fleet/FleetTone.kt` holds the state words
and dot colours.

**View model / state.** `ui/chat/ChatViewModel.kt` — `refreshFleet` `:420`,
`openFleet` `:450` (bounded by `FLEET_PANEL_SESSION_LIMIT = 24` at `:687`),
`toggleFamily` `:463`, `openChildTranscript` `:480`, and a `closeOverlay` that
cancels the descendant subscription. `ui/state/AppUiState.kt` gains
`FleetState`, `ChildTranscriptState`, `familyToggles`, `Overlay.Fleet` and
`Overlay.ChildTranscript`.

**Fake daemon.** `ui/daemon/FakeDaemonService.kt` — `FakeScenario.Fleet` `:344`,
`fleet()` `:738`, `subagents()` `:767`, `fleetRoster()` `:1003`, the bounded
`fleetSnapshot()` `:1133`, the complete `swarmSnapshot()` `:1214`, plus
`fleetOverride` / `subagentOverride` for the refused paths.

### The decisions worth arguing about

**Nesting wins over grouping; attention is preserved by promoting the family.**
A child parked on a human is tier 0 while its running parent is tier 1, so
nesting "within a section only" would split a family across two headers. A
family's tier is its *strongest member's* (`SessionTree.familyTier`), so the
family sorts into NEEDS YOU together and the parent's aggregate dot says why.

**The freeze had to become family-aware.** `OrderSnapshot.capture` ranks a flat
list; a family captured through it would be pinned in the section its root alone
belongs to and would jump on first render. `SessionTree.captureOrder` captures
the order the drawer actually draws.

**An undelegated roster must be identical to today.** `SessionTree.sections` is
pinned to reproduce `SessionListState.groups` exactly when no row names a
parent — that is what keeps every existing drawer golden honest.

**A chip cannot open a transcript on its own.** `ObserveSubagentWire` carries no
session id; the pairing exists only in a `session.fleet` snapshot, so a chip for
an agent the bounded snapshot omitted opens the **panel**, never a guessed
session. The fixture deliberately contains such an agent.

**The child screen has no composer.** Messaging a child is `agent.message`
addressed by `(parent session, agent)`, which this lane does not ship. The
child's own input-required card *is* rendered, answered against the child's own
compare-and-set coordinates, because a child parked on a human is otherwise
stuck.

**Absence never becomes zero.** `folded_children` is the one field where absence
really is zero (`is_zero_u32` skips it); everything else — usage, completeness,
`parent_agent_id`, `provider`, `callsign` — stays null and renders as "no data",
"unknown", a marked id fallback, or no mark at all.

### Clean-code pass (in scope)

- `SessionDrawer.kt`: dropped four dead imports (`ForgeChip`, `Arrangement`,
  `Icons.Rounded.Palette`, `Icons.Rounded.Bolt`) and the dead constants
  `FILTER_THRESHOLD` / `SEARCH_THRESHOLD` left over from the removed filter
  chips; `DrawerFooter` lost three unused parameters and an unused `val type`.
- **Left alone on purpose:** `SessionDrawer`'s public `appVersion`, `onFilter`
  and `themeMode` parameters are still unused by the body. Removing them would
  collide with round 12's edits to `HaiderApp.kt`. Flagged, not deleted.

### Tests and pins

`SessionTreeTest` — undelegated equivalence with `SessionListState.groups` and
with the flat capture; child under parent in tree order; orphan / self-parent /
cycle stay visible roots; a child needing a human pulls its family into NEEDS
YOU; aggregate dot is the strongest descendant state; count covers every
descendant; collapse threshold and folded rendering; last-child marking; frozen
family keeps position when a child parks; unseen family appended canonically;
a search matching only the child keeps the child visible.

`FleetWireShapeTest` — golden frames shaped as `frame.rs` serialises them:
`folded_children > 0` with `children: []` is bounded, not a leaf; an omitted
`folded_children` really is zero; completeness tri-state; absent usage stays
null and elapsed is measured at `generated_at_ms`; unrecognised state keeps its
raw word and unpublished is not the same as unknown; callsign fallback marked
and shortened; a chip carries no session id so unpaired agents stay
unaddressable; an unwired seam is `Unavailable`, not an empty tree.

`FleetUiTest` — drawer nesting, count control, folded/unfolded, a folded parent
still *speaks* the aggregate state, childless rows draw no control; strip
present with chips and absent on an ordinary session; chip opens the child's own
transcript; unpaired chip opens the panel; child transcript has no
send/stop/attach and issues no `chat.send`; child names its parent and back
returns; panel lists active children across sessions and a row jumps to that
session; panel states the bounded limits, the folded count and the refused read.

Goldens (both themes, `-Proborazzi.test.record=true`): `fleet-session-*`,
`fleet-drawer-*`, `fleet-child-*`, `fleet-panel-*`, `fleet-panel-empty-*`. The
panel is photographed through `FleetPanelContent`, because a `ModalBottomSheet`
renders in its own window and `onRoot()` cannot name one node for it.

### Repairs inside round 1

- **The only door into the panel was off-screen.** The strip's "all agents"
  control started at the *end* of the horizontally scrolling row; two chips are
  enough to push it past a 412 dp phone, which the third chip's pin caught (a
  tap aimed at a node whose centre is outside the viewport lands on nothing and
  reports no failure). The control is now fixed and first, a chip's task text is
  capped at `ForgeSize.subagentTaskMax` (116 dp) instead of the transcript's
  220 dp, and the tests scroll the strip before tapping.
- **The child screen sat under the status bar.** It returns early from
  `HaiderApp`, so the scaffold's `windowInsetsPadding(WindowInsets.safeDrawing)`
  — which lives on a Column that branch never reaches — did not apply. It now
  owns its own insets, the way `SettingsScreen` and `AccountsScreen` do. The
  Robolectric qualifier has no status-bar inset, so no golden shows this; it is
  a device-visible fix and is called out for the device pass.

### Gates

Machine gate markers: `runtime/machine-gate-markers.sh` -> `CLEAR`.
Every Gradle invocation went through `runtime/build-slot.sh` (single machine-wide
slot; the run queued behind a `972-cpu` quiet window and three peer lanes).

| Run | Command | Result |
| --- | --- | --- |
| compile | `:app:compileReleaseUnitTestKotlin` | **BUILD SUCCESSFUL** (exit 0) |
| gate 1 | `:app:testReleaseUnitTest :app:assembleRelease` | **BUILD FAILED** — 495 tests, 1 failed (`FleetUiTest > a chip the fleet snapshot never paired opens the panel, not a session`: the off-screen chip above) |
| gate 2 | `:app:testReleaseUnitTest :app:assembleRelease` | **BUILD SUCCESSFUL** — 495 tests, 0 failures, `assembleRelease` executed |
| goldens | `-Proborazzi.test.record=true :app:testReleaseUnitTest --tests FleetScreenshotTest` | **BUILD SUCCESSFUL**, 10 PNGs written |
| gate 3 (final) | `-Proborazzi.test.record=true :app:testReleaseUnitTest :app:assembleRelease` | **BUILD SUCCESSFUL** — 495 tests, 0 failures, `app-release-unsigned.apk` produced |

Sweeps and baseline checks are inside `testReleaseUnitTest` and all pass at the
final candidate: `DpLiteralSweepTest`, `TouchTargetSweepTest` (extended here
with four delegation surfaces), `MonospaceSweepTest`, `ShoutSweepTest`,
`AlphaLayerSweepTest`, `ContrastTest` — zero exemptions added to any of them.

**The strongest single signal:** the final run recorded with
`roborazzi.test.record=true`, and `git status` reports the 38 pre-existing
goldens as unchanged — byte-identical. An undelegated roster renders exactly as
it did before this lane touched the drawer's list construction.

### Not verified by this lane

- No device or emulator run: reserved for Astra.
- Roborazzi **verification** is not run (the suite records; CI comparison is a
  follow-up per UI-SPEC 6.4).
- No live daemon: `session.fleet` and the observe `subagents` list are exercised
  against the fake and against hand-written frames shaped from `frame.rs`.
- `agent.message` / `agent.cancel` are deliberately **not** shipped; the desktop
  panel's message composer and per-node Cancel have no counterpart here.
