package ai.diffforge.haider.ui.theme

import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.Easing
import androidx.compose.ui.unit.dp

/**
 * Spacing, size, motion and elevation tokens (UI-SPEC 2.3-2.5).
 *
 * Law: no `.dp` literal may appear in a layout modifier outside [ForgeSpace],
 * [ForgeSize] and [ForgeShapes]. This file is the single place those numbers live.
 */
object ForgeSpace {
    /** 4 dp base grid, with two half-steps reserved for optical work. */
    val xxs = 2.dp
    val xs = 4.dp
    val sm = 6.dp
    val md = 8.dp
    val lg = 12.dp
    val xl = 16.dp
    val xxl = 20.dp
    val xxxl = 24.dp
    val huge = 32.dp
}

object ForgeSize {
    /** Minimum interactive box, everywhere, no exceptions. */
    val touch = 48.dp

    /** Visible circle of an icon button inside a [touch] box. */
    val control = 44.dp
    val icon = 22.dp
    val iconMd = 20.dp
    val iconSm = 16.dp
    /** Chevrons and other secondary marks (round 10, R1/R2). */
    val iconXs = 14.dp
    val avatar = 18.dp

    /** Session-row state rail. */
    val rail = 3.dp
    val hairline = 1.dp
    /** The slim control row (H1), tightened to the target height (round 10). */
    val header = 48.dp
    /** Below this the header state pill drops its word and keeps its mark. */
    val statePillWordMin = 380.dp
    /**
     * A header control's painted size: 32 dp inside a 48 dp target.
     *
     * The owner's note was that the controls read too big. They do not get
     * smaller *targets* — `minimumInteractiveComponentSize` grows a touch
     * delegate the sweep cannot see — so the ink shrinks and the 48 dp
     * clickable box stays (round 10, R1).
     */
    val headerControl = 32.dp
    /** A segment's painted height inside the header's pill (reference 26 px). */
    val headerSegment = 26.dp
    /** The reference empty-state icon tile (dashboard.js:39192 — 44 px). */
    val emptyTile = 44.dp
    /** The composer field. 52 px is a desktop measure; 48 dp reads right here. */
    val composerField = 48.dp
    /** The attach / mic / send circles inside the field (round 10, R2). */
    val composerCircle = 32.dp
    val drawerWidth = 322.dp
    val drawerInset = 56.dp
    val rowMin = 64.dp
    /** A drawer session row's painted band inside its 48 dp target (R3). */
    val rowVisual = 40.dp

    /**
     * One delegation level's indent in the drawer, and the elbow the connector
     * draws into the child row. 12 dp keeps a depth-3 family readable inside a
     * 322 dp drawer, where the desktop's 16 px would not.
     */
    val treeIndent = 12.dp
    val treeElbow = 6.dp

    /** The family count/aggregate control's ink, inside its 48 dp target. */
    val familyPill = 26.dp

    /** A subagent chip's ink on the session header strip. */
    val subagentChip = 28.dp

    /**
     * How much of a subagent's task one chip may show. A chip is a glance, not
     * a row: at the transcript's 220 dp two chips already fill a 412 dp phone
     * and the third sits off-screen entirely.
     */
    val subagentTaskMax = 116.dp

    /** The fleet panel's own scroll ceiling, so a sheet cannot eat the screen. */
    val fleetListMax = 360.dp
    /** The merged daemon + New chat row's ink (R3). */
    val drawerRow = 44.dp
    /** The drawer search field's ink (R3). */
    val searchField = 40.dp
    /** A tool row's painted height (R4). */
    val toolRowHeight = 36.dp
    /** The cross-session strip's ink (R4). */
    val stripVisual = 40.dp
    val rowMinThreeLine = 82.dp
    val composerMin = 56.dp
    val contextRow = 32.dp
    val stateDot = 6.dp
    val badgeDot = 9.dp
    /** The brand mark inside the 22 dp avatar slot (modelBrand.jsx: 13 px). */
    val brandMark = 14.dp
    /** The activity badge inside its ring (modelBrand.jsx: 5 px). */
    val activityDot = 5.dp
    /** The 24x24 viewport every ported vendor mark is drawn on. */
    val markViewport = 24.dp

    // ---------- Haider Code wordmark (lane 971-ui-logo) ----------

    /** The 64x23 pixel grid the wordmark is drawn on (haidercode-web logo.svg). */
    val logoViewportWidth = 64.dp
    val logoViewportHeight = 23.dp
    /** The wordmark at the top of the start surface. */
    val logoStart = 128.dp
    /** The wordmark in the drawer's identity row. */
    val logoDrawer = 96.dp
    /** The wordmark preview beside the Settings logo-style choice. */
    val logoSettings = 112.dp
    /** An attachment thumbnail in the transcript or the composer strip. */
    val thumbnail = 56.dp
    /** The name column inside a file tile. */
    val attachmentLabel = 92.dp
    /** The streaming caret's drawn bar (verify-11 O5). */
    val caretWidth = 2.dp
    val caretHeight = 20.dp
    /** Chip and composer-select ink: 30 dp inside a 48 dp target (R2). */
    val chip = 30.dp
    val segmented = 36.dp
    val bannerAction = 34.dp
    val footerRow = 48.dp
    val newSessionRow = 46.dp
    val actionButton = 44.dp
    val stopChip = 28.dp

    // ---------- workflow DAG canvas (lane 971-UI-workflows) ----------

    /**
     * A node card. Wide enough for an uppercase graph node name plus its phase
     * word at the app's own scale, and 62 dp tall so the card is its own target
     * without a separate touch box: a DAG node is a thing you tap.
     */
    val graphNodeWidth = 148.dp
    val graphNodeHeight = 62.dp

    /** Between siblings in one layer, and between layers. */
    val graphColumnGap = 20.dp
    val graphLayerGap = 44.dp

    /** Breathing room around the whole graph, and where an input stub is drawn. */
    val graphCanvasPad = 28.dp

    val graphEdge = 2.dp
    val graphDash = 6.dp
    val graphDashGap = 5.dp
    val graphArrow = 7.dp

    /** The phase dot on a node card, and the accent bar down its left edge. */
    val graphPhaseDot = 8.dp
    val graphNodeRail = 3.dp

    /** One level of indent in the activation-AST tree. */
    val astIndent = 14.dp

    /** The agent-type colour/glyph tile in the Looms list. */
    val loomGlyphTile = 34.dp

    /** The authoring editor's minimum height, so a draft is readable at once. */
    val authoringEditorMin = 200.dp

    /** Tablet affordance retained from the existing transcript. */
    val readableMax = 776.dp
    val sheetMax = 640.dp
    val proseMax = 600.dp
    val startFirstChildInset = 22.dp
    val toolResultMax = 220.dp
    val progressLine = 2.dp
}

object ForgeMotion {
    const val FAST_MS = 120
    const val BASE_MS = 200
    const val SLOW_MS = 320

    /** FastOutSlowIn, spelled out so this file has no Compose-animation import cycle. */
    val easing: Easing = CubicBezierEasing(0.4f, 0.0f, 0.2f, 1.0f)

    /** Session-row marquee cycle. */
    const val MARQUEE_MS = 900

    /** Drawer relative-time ticker. */
    const val TIME_TICK_MS = 30_000L

    /** Daemon resource line refresh while the drawer is open. */
    const val RESOURCE_TICK_MS = 5_000L
}

/**
 * Width thresholds, declared here for the same reason every other dimension is:
 * a breakpoint spelled inline is a dimension nothing can sweep.
 */
object ForgeBreakpoint {
    /**
     * Below this the composer's three selects drop their labels.
     *
     * The row is the screen minus its gutter, so a 360 dp phone measures 336
     * here and a 412 dp one measures 388: this threshold takes the labels off
     * the narrow phone and leaves them on the wide one. At 336 a third of the
     * row is about 108 dp, which holds a label OR a value but not both, so the
     * *value* ellipsised and the current permission mode was unreadable
     * without opening its own picker (971-V F8). Label-less chips are also
     * what the declutter pass asked for (addition F, S5); the label stays in
     * `contentDescription`, so nothing is lost to a screen reader.
     */
    val compactSelects = 360.dp
}

object ForgeElevation {
    /**
     * Elevation is a surface step, not a shadow, except for the drawer which
     * overlaps same-coloured content (UI-SPEC 2.5).
     */
    val drawer = 14.dp
    const val DRAWER_SHADOW_DARK = 0.45f
    const val DRAWER_SHADOW_LIGHT = 0.16f
}
